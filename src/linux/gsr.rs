//! gpu-screen-recorder, driven directly (not through `omarchy-capture-screenrecording`, which
//! trims the first 0.1 s and mutes/normalizes audio).
//!
//! `-write-first-frame-ts yes` writes `<out>.ts` with the first frame's `CLOCK_MONOTONIC` µs:
//! that is `video_offset_ms`'s source. `-ipc` gives a clean `stop` that replies once the file is
//! written. argv[0] stays `gpu-screen-recorder`, so Omarchy's bar indicator (`pgrep -f
//! ^gpu-screen-recorder`) lights up, and Omarchy's own stop (SIGINT to that pattern) still
//! saves the file, which we treat as "stopped by the user".

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::model::{CaptureMode, TargetInfo};

pub struct Recorder {
    child: std::sync::Mutex<Child>,
    out: PathBuf,
    ipc: PathBuf,
}

pub struct Finished {
    pub path: PathBuf,
    pub first_frame_mono_us: Option<u64>,
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("scrawl")
}

/// Whether some gpu-screen-recorder (Omarchy's, or a stale one of ours) is already running.
/// Another gpu-screen-recorder is running (Omarchy's screen recording, say), apart from a
/// `scrawl screencast`, which is meant to run alongside.
pub fn other_running() -> bool {
    let casts = crate::linux::screencast::pids();
    Command::new("pgrep")
        .args(["-f", "^gpu-screen-recorder"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .any(|pid| !casts.contains(&pid))
        })
}

fn capture_args(target: &TargetInfo) -> Result<Vec<String>> {
    Ok(match target.mode {
        CaptureMode::Display => {
            let mons = crate::linux::hypr::monitors()?;
            let m = mons
                .iter()
                .find(|m| m.id.max(0) as u32 == target.display_id)
                .or_else(|| mons.iter().find(|m| m.contains(target.frame.x + 1.0, target.frame.y + 1.0)))
                .context("Monitor for the capture is gone")?;
            vec!["-w".into(), m.name.clone()]
        }
        CaptureMode::Window | CaptureMode::Region => {
            let f = target.frame;
            // gsr takes region geometry in the compositor's logical (layout) px, like slurp.
            vec![
                "-w".into(),
                "region".into(),
                "-region".into(),
                format!(
                    "{}x{}+{}+{}",
                    f.width.round() as i64,
                    f.height.round() as i64,
                    f.x.round() as i64,
                    f.y.round() as i64
                ),
            ]
        }
        CaptureMode::Device => bail!("Device capture is macOS-only"),
    })
}

impl Recorder {
    pub fn start(target: &TargetInfo, out: &Path) -> Result<Self> {
        if other_running() {
            bail!("Another screen recording is running (gpu-screen-recorder); stop it first");
        }
        let dir = runtime_dir();
        std::fs::create_dir_all(&dir)?;
        let ipc = dir.join(format!("gsr-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&ipc);
        let _ = std::fs::remove_file(ts_path(out));
        let mut args = capture_args(target)?;
        let mic = std::env::var("SCRAWL_AUDIO_DEVICE").unwrap_or_else(|_| "default_input".into());
        args.extend(
            [
                "-f", "60", "-fm", "cfr", "-k", "auto", "-ac", "aac", "-a", &mic,
                "-fallback-cpu-encoding", "yes", "-write-first-frame-ts", "yes",
            ]
            .map(String::from),
        );
        args.push("-ipc".into());
        args.push(ipc.display().to_string());
        args.push("-o".into());
        args.push(out.display().to_string());
        let log = std::fs::File::create(dir.join("gsr.log"))?;
        crate::capture::scrawl_log(format!("gpu-screen-recorder {}", args.join(" ")));
        let child = Command::new("gpu-screen-recorder")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("Could not start gpu-screen-recorder (is it installed?)")?;
        let rec = Self {
            child: std::sync::Mutex::new(child),
            out: out.to_path_buf(),
            ipc,
        };
        // Fail fast if it dies during startup (no monitor, encoder failure…).
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            if let Some(status) = rec.child()?.try_wait()? {
                let log = std::fs::read_to_string(dir.join("gsr.log")).unwrap_or_default();
                bail!(
                    "gpu-screen-recorder exited ({status}): {}",
                    log.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(rec)
    }

    fn child(&self) -> Result<std::sync::MutexGuard<'_, Child>> {
        Ok(self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub fn is_running(&self) -> bool {
        self.child().is_ok_and(|mut c| matches!(c.try_wait(), Ok(None)))
    }

    pub fn stop(self) -> Result<Finished> {
        let mut saved: Option<PathBuf> = None;
        if self.is_running() {
            match ipc_stop(&self.ipc) {
                Ok(path) => saved = Some(path),
                Err(e) => {
                    crate::capture::scrawl_log(format!("gsr IPC stop failed ({e:#}); sending SIGINT"));
                    unsafe { libc::kill(self.child()?.id() as i32, libc::SIGINT) };
                }
            }
        } else {
            crate::capture::scrawl_log("gpu-screen-recorder had already exited (stopped elsewhere)");
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self.child()?.try_wait()?.is_some() {
                break;
            }
            if Instant::now() > deadline {
                let mut c = self.child()?;
                let _ = c.kill();
                let _ = c.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&self.ipc);
        let path = saved.unwrap_or_else(|| self.out.clone());
        if !path.is_file() {
            bail!("gpu-screen-recorder left no file at {}", path.display());
        }
        let first = read_first_frame(&path);
        let _ = std::fs::remove_file(ts_path(&path));
        Ok(Finished {
            path,
            first_frame_mono_us: first,
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let mut c = self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(c.try_wait(), Ok(None)) {
            unsafe { libc::kill(c.id() as i32, libc::SIGINT) };
        }
    }
}

fn ts_path(video: &Path) -> PathBuf {
    let mut s = video.as_os_str().to_owned();
    s.push(".ts");
    PathBuf::from(s)
}

/// `<video>.ts` holds a header line and `monotonic_microsec realtime_microsec`.
fn read_first_frame(video: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(ts_path(video)).ok()?;
    text.lines()
        .filter_map(|l| l.split_whitespace().next()?.parse::<u64>().ok())
        .next()
}

fn ipc_stop(sock: &Path) -> Result<PathBuf> {
    let mut s = UnixStream::connect(sock).context("connect gsr IPC")?;
    s.set_read_timeout(Some(Duration::from_secs(20)))?;
    s.write_all(b"{\"id\":1,\"name\":\"stop\"}\n")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(&line).context("bad gsr IPC reply")?;
    if v["result"] != "ok" {
        bail!("gsr stop: {}", v["data"]);
    }
    Ok(v["data"].as_str().map(PathBuf::from).context("gsr stop reply had no path")?)
}

/// The mic track (first audio stream), mono 16 kHz, with `offset_ms` of leading silence so the
/// WAV starts at bundle time 0 like upstream's `audio.wav`.
pub fn extract_audio(video: &Path, wav: &Path, offset_ms: u64) -> Result<()> {
    let af = format!("adelay=delays={offset_ms}:all=1,aresample=16000");
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin", "-i"])
        .arg(video)
        .args(["-map", "0:a:0", "-ac", "1", "-af", &af, "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(wav)
        .output()
        .context("Could not run ffmpeg")?;
    if !out.status.success() {
        bail!("ffmpeg audio extract: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_first_frame_file() {
        let dir = std::env::temp_dir().join(format!("scrawl-gsr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("rec.mp4");
        std::fs::write(super::ts_path(&video), "monotonic_microsec\trealtime_microsec\n1434851294904\t1792387743661915\n").unwrap();
        assert_eq!(super::read_first_frame(&video), Some(1_434_851_294_904));
        let _ = std::fs::remove_dir_all(dir);
    }
}
