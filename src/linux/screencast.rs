//! `scrawl screencast start [MONITOR|all] [--no-mic]`, `stop`, `status`: a plain recording of the
//! desktop (for demos of Scrawl itself), started and stopped from the command line so an agent can
//! run it. gpu-screen-recorder, one file per monitor (a region across monitors comes out garbled),
//! saved in the Videos folder. Scrawl's own recordings run alongside: `gsr::other_running` ignores
//! these recorders.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::linux::{desktop, gsr, hypr};

#[derive(Serialize, Deserialize)]
struct Cast {
    pid: u32,
    /// The process's start time (`/proc/PID/stat`), so a pid reused after the recorder exited is
    /// never mistaken for it. 0 in state files from before it was kept: never matches.
    #[serde(default)]
    start: u64,
    monitor: String,
    path: PathBuf,
}

fn state_path() -> PathBuf {
    gsr::runtime_dir().join("screencast.json")
}

/// When `pid` started, in clock ticks since boot (field 22 of `/proc/PID/stat`).
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may hold spaces or parentheses; fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

impl Cast {
    fn alive(&self) -> bool {
        self.start != 0 && start_time(self.pid) == Some(self.start)
    }

    /// Signals the recorder only if it is still the process that was started: a pidfd pins the
    /// process, then its start time is checked.
    fn signal(&self, sig: libc::c_int) -> bool {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, self.pid as libc::pid_t, 0) };
        if fd < 0 {
            return false;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        self.alive()
            && unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0)
            } == 0
    }
}

/// Written beside the old state and renamed over it: a failed write never loses the recorders
/// already on record.
fn save(casts: &[Cast]) -> Result<()> {
    let tmp = state_path().with_extension("json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(casts)?)?;
    std::fs::rename(&tmp, state_path())?;
    Ok(())
}

/// Held for a whole start or stop, so two commands at once can't both start recorders (and keep
/// only one set on record) or remove the state another one just wrote.
fn lock() -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(gsr::runtime_dir())?;
    let file = std::fs::File::create(gsr::runtime_dir().join("screencast.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!("could not lock the screencast state: {}", std::io::Error::last_os_error());
    }
    Ok(file)
}

fn load() -> Vec<Cast> {
    std::fs::read(state_path())
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

/// The screencast recorders running now, which Scrawl's recorder check leaves alone.
pub fn pids() -> Vec<u32> {
    load().into_iter().filter(Cast::alive).map(|c| c.pid).collect()
}

fn stamp() -> String {
    Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| crate::make_bundle_id())
}

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => {
            let _lock = lock()?;
            start(&args[1..])
        }
        Some("stop") => {
            let _lock = lock()?;
            stop()
        }
        Some("status") | None => {
            let casts: Vec<Cast> = load().into_iter().filter(Cast::alive).collect();
            if casts.is_empty() {
                println!("not recording");
            }
            for c in casts {
                println!("recording {} → {}", c.monitor, c.path.display());
            }
            Ok(())
        }
        Some(other) => bail!("usage: scrawl screencast start [MONITOR|all] [--no-mic] | stop | status (not {other})"),
    }
}

fn start(args: &[String]) -> Result<()> {
    if !pids().is_empty() {
        bail!("a screencast is already recording (scrawl screencast stop)");
    }
    let monitors = hypr::monitors()?;
    let which = args.iter().find(|a| !a.starts_with("--")).map(String::as_str);
    let chosen: Vec<_> = match which {
        Some("all") => monitors.iter().collect(),
        Some(name) => monitors.iter().filter(|m| m.name == name).collect(),
        None => monitors.iter().filter(|m| m.focused).take(1).collect(),
    };
    if chosen.is_empty() {
        bail!(
            "no such monitor; have {}",
            monitors.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    let mic = !args.iter().any(|a| a == "--no-mic");
    let dir = dirs::video_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Videos"))
        .join("hyprscrawl");
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(gsr::runtime_dir())?;
    let stamp = stamp();
    let mut children: Vec<(std::process::Child, PathBuf)> = Vec::new();
    let started = start_recorders(&chosen, mic, &dir, &stamp, &mut children)
        .and_then(|casts| save(&casts).map(|()| casts));
    let casts = match started {
        Ok(casts) => casts,
        Err(e) => {
            // Nothing half-started is left running, or recording without a record of it.
            for (child, _) in &mut children {
                // Only one not yet reaped: its pid can't have been reused.
                if matches!(child.try_wait(), Ok(None)) {
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
                }
            }
            for (mut child, path) in children {
                let deadline = Instant::now() + Duration::from_secs(5);
                while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(path);
            }
            return Err(e);
        }
    };
    desktop::refresh_indicators();
    for c in &casts {
        println!("recording {} → {}", c.monitor, c.path.display());
    }
    Ok(())
}

/// Starts one recorder per monitor; every child started goes into `children` (also on error).
fn start_recorders(
    chosen: &[&hypr::Monitor],
    mic: bool,
    dir: &std::path::Path,
    stamp: &str,
    children: &mut Vec<(std::process::Child, PathBuf)>,
) -> Result<Vec<Cast>> {
    let mut casts = Vec::new();
    for (i, m) in chosen.iter().enumerate() {
        let path = dir.join(format!("screencast-{stamp}-{}.mp4", m.name));
        let log = std::fs::File::create(gsr::runtime_dir().join(format!("screencast-{}.log", m.name)))?;
        let mut cmd = Command::new("gpu-screen-recorder");
        cmd.args(["-w", &m.name, "-f", "60", "-k", "h264", "-fallback-cpu-encoding", "yes"]);
        // One microphone track, on the first file.
        if mic && i == 0 {
            let device = std::env::var("SCRAWL_AUDIO_DEVICE").unwrap_or_else(|_| "default_input".into());
            cmd.args(["-ac", "aac", "-a", &device]);
        }
        let child = cmd
            .arg("-o")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group: it outlives this command and a Ctrl+C here.
            .process_group(0)
            .spawn()
            .context("Could not run gpu-screen-recorder")?;
        let pid = child.id();
        children.push((child, path.clone()));
        casts.push(Cast {
            pid,
            start: start_time(pid).context("gpu-screen-recorder vanished as it started")?,
            monitor: m.name.clone(),
            path,
        });
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(600) {
            if let Some(status) = children.last_mut().unwrap().0.try_wait()? {
                let tail = std::fs::read_to_string(gsr::runtime_dir().join(format!("screencast-{}.log", m.name)))
                    .unwrap_or_default();
                bail!("gpu-screen-recorder exited ({status}): {}", tail.lines().rev().take(3).collect::<Vec<_>>().join(" | "));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(casts)
}

fn stop() -> Result<()> {
    let casts = load();
    if !casts.iter().any(Cast::alive) {
        let _ = std::fs::remove_file(state_path());
        bail!("no screencast is recording");
    }
    for c in &casts {
        c.signal(libc::SIGINT);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while casts.iter().any(Cast::alive) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // A recorder that ignored the stop stays on record, so another stop (or status) still sees it.
    let (running, casts): (Vec<Cast>, Vec<Cast>) = casts.into_iter().partition(Cast::alive);
    if running.is_empty() {
        let _ = std::fs::remove_file(state_path());
    } else {
        save(&running)?;
    }
    desktop::refresh_indicators();
    for c in &casts {
        let length = crate::helper::probe(&c.path)
            .map(|p| format!("{:.1} s", p.duration_ms as f64 / 1000.0))
            .unwrap_or_else(|_| "unreadable".into());
        println!("saved {} ({length}) → {}", c.monitor, c.path.display());
    }
    if !running.is_empty() {
        let list: Vec<String> = running.iter().map(|c| format!("{} (pid {})", c.monitor, c.pid)).collect();
        bail!("still recording after 15 s: {}; try `scrawl screencast stop` again", list.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_only_with_the_same_start_time() {
        let me = std::process::id();
        let start = start_time(me).expect("own start time");
        let path = PathBuf::new();
        let cast = |start| Cast { pid: me, start, monitor: String::new(), path: path.clone() };
        assert!(cast(start).alive());
        assert!(!cast(start + 1).alive(), "a reused pid has another start time");
        assert!(!cast(0).alive(), "old state files never match");
        assert!(!cast(start + 1).signal(0), "and is never signalled");
        assert!(cast(start).signal(0));
    }
}
