//! Linux stand-in for `scrawl-helper` (see `src/helper.rs` upstream): the same functions and
//! types, backed by `ffprobe`/`ffmpeg` for media and `whisper-cli` (whisper.cpp) for
//! transcription. Swapped in for `helper.rs` by `#[cfg_attr(target_os = "linux", path)]`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::model::{RgbaFrame, TranscriptData};

/// There is no Swift helper on Linux. Callers that need one (the AVFoundation export renderer,
/// onboarding) report this.
pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SCRAWL_HELPER") {
        return Ok(PathBuf::from(path));
    }
    bail!("scrawl-helper is macOS-only; this Linux build uses ffmpeg and whisper-cli instead")
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PhoneDevice {
    pub id: String,
    pub name: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
}

/// Linux has no TCC: report nothing granted or denied.
pub fn permissions()
-> Result<std::collections::BTreeMap<crate::onboarding::Permission, crate::onboarding::Status>> {
    Ok(Default::default())
}

/// Cabled iPhone capture is macOS-only (CoreMediaIO).
pub fn devices() -> Result<Vec<PhoneDevice>> {
    Ok(Vec::new())
}

/// Runs a command to completion with a timeout, returning stdout.
fn output(program: &str, args: &[&std::ffi::OsStr], timeout: Duration) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not start {program} (is it installed?)"))?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let out_reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let err_reader = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        err
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        bail!(
            "{program} failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(out)
}

fn os(s: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(s)
}

/// Lowers a child's priority the way upstream runs the helper under `taskpolicy -c utility`.
fn nice_args() -> Vec<String> {
    vec!["-n".into(), "10".into()]
}

/// Fills upstream's default (Apple SpeechAnalyzer) engine slot, so `transcribe.rs` is unchanged:
/// ChatGPT when chosen (`"transcriber": "chatgpt"`), with whisper.cpp as its timing source and its
/// fallback; otherwise whisper.cpp alone. A transcription `prefetch`ed for this file is used.
pub fn transcribe(audio: &Path) -> Result<TranscriptData> {
    let prefetched = PREFETCH.lock().unwrap().take_if(|p| p.wav == audio);
    if let Some(prefetched) = prefetched {
        let result = match prefetched.job {
            Job::Cloud(pending) => pending.finish(crate::linux::chatgpt::whisper_grace()),
            Job::Local(handle) => handle.join().unwrap_or_else(|_| Err(anyhow::anyhow!("whisper thread panicked"))),
        };
        let _ = std::fs::remove_file(&prefetched.copy);
        match result {
            Ok(transcript) => return Ok(transcript),
            Err(error) => crate::capture::scrawl_log(format!("prefetched transcription failed, using whisper.cpp: {error:#}")),
        }
        return crate::linux::whisper::transcribe(audio);
    }
    if crate::linux::chatgpt::enabled() {
        match crate::linux::chatgpt::transcribe(audio) {
            Ok(transcript) => return Ok(transcript),
            Err(error) => crate::capture::scrawl_log(format!("ChatGPT transcription failed, using whisper.cpp: {error:#}")),
        }
    }
    crate::linux::whisper::transcribe(audio)
}

enum Job {
    Cloud(crate::linux::chatgpt::Pending),
    Local(std::thread::JoinHandle<Result<TranscriptData>>),
}

struct Prefetch {
    /// The path `transcribe` will be asked about.
    wav: PathBuf,
    /// What the job actually reads: upstream rewrites `wav` while finishing the bundle.
    copy: PathBuf,
    job: Job,
}

static PREFETCH: std::sync::Mutex<Option<Prefetch>> = std::sync::Mutex::new(None);

/// Starts transcribing `wav` now, so it runs while the candidate frames decode; the bundle's
/// later `transcribe(wav)` picks up the result.
pub fn prefetch(wav: &Path) -> Result<()> {
    let dir = crate::linux::gsr::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(format!("prefetch-{}.wav", crate::make_bundle_id()));
    std::fs::copy(wav, &copy)?;
    let local = |copy: PathBuf| Job::Local(std::thread::spawn(move || crate::linux::whisper::transcribe(&copy)));
    let job = if crate::linux::chatgpt::enabled() {
        match crate::linux::chatgpt::start(&copy) {
            Ok(pending) => Job::Cloud(pending),
            Err(error) => {
                crate::capture::scrawl_log(format!("ChatGPT transcription unavailable, using whisper.cpp: {error:#}"));
                local(copy.clone())
            }
        }
    } else {
        local(copy.clone())
    };
    let old = PREFETCH.lock().unwrap().replace(Prefetch {
        wav: wav.to_owned(),
        copy,
        job,
    });
    if let Some(old) = old {
        let _ = std::fs::remove_file(old.copy);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub duration_ms: u64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub audio_tracks: u32,
    /// The video track's nominal frame rate (0 when unknown).
    #[serde(default)]
    pub fps: f64,
}

pub fn probe(movie: &Path) -> Result<Probe> {
    let out = output(
        "ffprobe",
        &[
            os("-v"),
            os("error"),
            os("-show_streams"),
            os("-show_format"),
            os("-of"),
            os("json"),
            movie.as_os_str(),
        ],
        Duration::from_secs(30),
    )?;
    let v: serde_json::Value = serde_json::from_slice(&out).context("Invalid ffprobe output")?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams.iter().find(|s| s["codec_type"] == "video");
    let audio_tracks = streams.iter().filter(|s| s["codec_type"] == "audio").count() as u32;
    let secs = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse::<f64>().ok());
    let duration = secs(&v["format"]["duration"])
        .or_else(|| video.and_then(|s| secs(&s["duration"])))
        .context("ffprobe found no duration")?;
    let fps = video
        .and_then(|s| s["avg_frame_rate"].as_str().or(s["r_frame_rate"].as_str()))
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
            (d > 0.0).then(|| n / d)
        })
        .unwrap_or(0.0);
    Ok(Probe {
        duration_ms: (duration * 1000.0).round() as u64,
        width: video.and_then(|s| s["width"].as_u64()).unwrap_or(0) as u32,
        height: video.and_then(|s| s["height"].as_u64()).unwrap_or(0) as u32,
        audio_tracks,
        fps,
    })
}

/// Re-encodes `movie` into `out` as HEVC with its long edge at most `max_edge`, audio copied and
/// every sample at its original time.
pub fn shrink(movie: &Path, out: &Path, max_edge: u32) -> Result<crate::retention::Shrunk> {
    let started = std::time::Instant::now();
    let before = std::fs::metadata(movie)?.len();
    let p = probe(movie)?;
    let (w, h) = fit(p.width, p.height, max_edge);
    let scale = format!("scale={w}:{h}");
    let mut args: Vec<String> = nice_args();
    args.extend(
        [
            "ffmpeg", "-v", "error", "-y", "-i",
        ]
        .map(String::from),
    );
    args.push(movie.display().to_string());
    args.extend(
        [
            "-vf", &scale, "-c:v", "libx265", "-preset", "fast", "-crf", "28", "-tag:v", "hvc1",
            "-c:a", "copy", "-copyts", "-map", "0",
        ]
        .map(String::from),
    );
    args.push(out.display().to_string());
    let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| os(a)).collect();
    output("nice", &refs, Duration::from_secs(1800))?;
    Ok(crate::retention::Shrunk {
        before,
        after: std::fs::metadata(out)?.len(),
        width: w,
        height: h,
        ms: started.elapsed().as_millis() as u64,
    })
}

/// Mixes a movie's audio to 16 kHz mono WAV, aligned to the movie's start.
pub fn export_audio(movie: &Path, wav: &Path) -> Result<()> {
    output(
        "ffmpeg",
        &[
            os("-v"),
            os("error"),
            os("-y"),
            os("-i"),
            movie.as_os_str(),
            os("-map"),
            os("0:a:0"),
            os("-ac"),
            os("1"),
            os("-ar"),
            os("16000"),
            os("-c:a"),
            os("pcm_s16le"),
            wav.as_os_str(),
        ],
        Duration::from_secs(120),
    )?;
    Ok(())
}

pub enum FrameTimes<'a> {
    /// Nearest decodable frame every N ms, stamped with its actual time.
    Every(u64),
    /// Exactly these times.
    At(&'a [u64]),
}

/// Even-sided size with the long edge at most `max_edge` (0 keeps native).
fn fit(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let (w, h) = (width.max(2), height.max(2));
    let long = w.max(h);
    if max_edge == 0 || long <= max_edge {
        return (w & !1, h & !1);
    }
    let s = f64::from(max_edge) / f64::from(long);
    (
        ((f64::from(w) * s).round() as u32).max(2) & !1,
        ((f64::from(h) * s).round() as u32).max(2) & !1,
    )
}

fn decode_one(movie: &Path, pre_input: &[String], vf: &str, w: u32, h: u32) -> Result<Vec<Vec<u8>>> {
    decode(movie, pre_input, vf, &["-frames:v", "1"], w, h)
}

fn decode(
    movie: &Path,
    pre_input: &[String],
    vf: &str,
    post: &[&str],
    w: u32,
    h: u32,
) -> Result<Vec<Vec<u8>>> {
    let mut args: Vec<String> = vec!["-v".into(), "error".into(), "-nostdin".into()];
    args.extend(pre_input.iter().cloned());
    args.extend(["-i".into(), movie.display().to_string()]);
    args.extend(["-an", "-vf", vf].map(String::from));
    args.extend(post.iter().map(|s| s.to_string()));
    args.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "-"].map(String::from));
    let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| os(a)).collect();
    let raw = output("ffmpeg", &refs, Duration::from_secs(300))?;
    let size = w as usize * h as usize * 4;
    ensure!(size > 0, "Empty frame size");
    Ok(raw.chunks_exact(size).map(<[u8]>::to_vec).collect())
}

/// The nearest frame every `ms`, stamped with its time, handed to `each` as ffmpeg decodes it:
/// only one frame is in flight, so the caller decides what stays in memory (a long capture's
/// candidates would otherwise be held twice, raw and split, before any thinning).
pub fn frames_every(movie: &Path, ms: u64, max_edge: u32, mut each: impl FnMut(RgbaFrame)) -> Result<()> {
    let p = probe(movie)?;
    ensure!(p.width > 0 && p.height > 0, "No video stream in {}", movie.display());
    let (w, h) = fit(p.width, p.height, max_edge);
    let ms = ms.max(1);
    // fps=1000/ms samples the frame nearest each multiple of `ms` from the start.
    let vf = format!("fps=fps=1000/{ms}:round=near,scale={w}:{h}:flags=area");
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(movie)
        .args(["-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start ffmpeg (is it installed?)")?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let err_reader = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        err
    });
    // The same limit as the other decodes: past it ffmpeg is killed, which ends the reads.
    let child = std::sync::Arc::new(std::sync::Mutex::new(child));
    let finished = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let watchdog = {
        let (child, finished) = (std::sync::Arc::clone(&child), std::sync::Arc::clone(&finished));
        std::thread::spawn(move || {
            let (done, cv) = &*finished;
            let (done, timeout) = cv
                .wait_timeout_while(done.lock().unwrap(), Duration::from_secs(300), |done| !*done)
                .unwrap();
            if !*done && timeout.timed_out() {
                let _ = child.lock().unwrap().kill();
                return true;
            }
            false
        })
    };
    let size = w as usize * h as usize * 4;
    let mut i = 0_u64;
    loop {
        let mut rgba = vec![0_u8; size];
        if !read_full(&mut stdout, &mut rgba) {
            break;
        }
        each(RgbaFrame::new((i * ms).min(p.duration_ms), w, h, rgba));
        i += 1;
    }
    drop(stdout);
    // Polled, letting go of the lock in between: ffmpeg can close stdout and still hang, and
    // the watchdog needs the lock to kill it.
    let status = loop {
        match child.lock().unwrap().try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    *finished.0.lock().unwrap() = true;
    finished.1.notify_all();
    let timed_out = watchdog.join().unwrap_or(false);
    let err = err_reader.join().unwrap_or_default();
    ensure!(!timed_out, "ffmpeg timed out");
    if !status?.success() {
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        bail!("ffmpeg failed: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
    }
    Ok(())
}

/// Fills `buf` from `r`; false at the end of the stream (a partial last frame is dropped).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return false,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

/// Decodes movie frames into memory; `max_edge` 0 keeps native size.
pub fn frames(movie: &Path, times: FrameTimes, max_edge: u32) -> Result<Vec<RgbaFrame>> {
    let p = probe(movie)?;
    ensure!(p.width > 0 && p.height > 0, "No video stream in {}", movie.display());
    let (w, h) = fit(p.width, p.height, max_edge);
    let scale = format!("scale={w}:{h}:flags=area");
    match times {
        FrameTimes::Every(ms) => {
            let mut frames = Vec::new();
            frames_every(movie, ms, max_edge, |f| frames.push(f))?;
            Ok(frames)
        }
        FrameTimes::At(times) => {
            if times.is_empty() {
                return Ok(Vec::new());
            }
            let last = p.duration_ms.saturating_sub(34);
            let chunk = times.len().div_ceil(4).max(1);
            std::thread::scope(|scope| -> Result<Vec<RgbaFrame>> {
                let workers: Vec<_> = times
                    .chunks(chunk)
                    .map(|part| {
                        let scale = &scale;
                        scope.spawn(move || -> Result<Vec<RgbaFrame>> {
                            part.iter()
                                .map(|&t| {
                                    let at = t.min(last);
                                    // Input-side -ss seeks accurately (decodes from the
                                    // previous keyframe), then one frame is kept.
                                    let pre = vec!["-ss".to_string(), format!("{:.3}", at as f64 / 1000.0)];
                                    let mut raws = decode_one(movie, &pre, scale, w, h)?;
                                    ensure!(!raws.is_empty(), "No frame at {t} ms");
                                    Ok(RgbaFrame::new(t, w, h, raws.swap_remove(0)))
                                })
                                .collect()
                        })
                    })
                    .collect();
                let mut out = Vec::with_capacity(times.len());
                for worker in workers {
                    out.extend(
                        worker
                            .join()
                            .map_err(|_| anyhow::anyhow!("Frame decode worker panicked"))??,
                    );
                }
                Ok(out)
            })
        }
    }
}

/// Renders `text` as a compact Unicode QR code with `qrencode` when it is installed.
pub fn qr(text: &str) -> Result<String> {
    let out = output(
        "qrencode",
        &[os("-t"), os("UTF8"), os("-m"), os("1"), os(text)],
        Duration::from_secs(5),
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}
