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
    monitor: String,
    path: PathBuf,
}

fn state_path() -> PathBuf {
    gsr::runtime_dir().join("screencast.json")
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn load() -> Vec<Cast> {
    std::fs::read(state_path())
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

/// The screencast recorders running now, which Scrawl's recorder check leaves alone.
pub fn pids() -> Vec<u32> {
    load().into_iter().map(|c| c.pid).filter(|&p| alive(p)).collect()
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
        Some("start") => start(&args[1..]),
        Some("stop") => stop(),
        Some("status") | None => {
            let casts: Vec<Cast> = load().into_iter().filter(|c| alive(c.pid)).collect();
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
        let mut child = cmd
            .arg("-o")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group: it outlives this command and a Ctrl+C here.
            .process_group(0)
            .spawn()
            .context("Could not run gpu-screen-recorder")?;
        casts.push(Cast {
            pid: child.id(),
            monitor: m.name.clone(),
            path,
        });
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(600) {
            if let Some(status) = child.try_wait()? {
                let tail = std::fs::read_to_string(gsr::runtime_dir().join(format!("screencast-{}.log", m.name)))
                    .unwrap_or_default();
                for c in &casts {
                    unsafe { libc::kill(c.pid as libc::pid_t, libc::SIGINT) };
                }
                bail!("gpu-screen-recorder exited ({status}): {}", tail.lines().rev().take(3).collect::<Vec<_>>().join(" | "));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    std::fs::write(state_path(), serde_json::to_vec_pretty(&casts)?)?;
    desktop::refresh_indicators();
    for c in &casts {
        println!("recording {} → {}", c.monitor, c.path.display());
    }
    Ok(())
}

fn stop() -> Result<()> {
    let casts = load();
    if casts.iter().all(|c| !alive(c.pid)) {
        let _ = std::fs::remove_file(state_path());
        bail!("no screencast is recording");
    }
    for c in &casts {
        if alive(c.pid) {
            unsafe { libc::kill(c.pid as libc::pid_t, libc::SIGINT) };
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while casts.iter().any(|c| alive(c.pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(state_path());
    desktop::refresh_indicators();
    for c in &casts {
        let length = crate::helper::probe(&c.path)
            .map(|p| format!("{:.1} s", p.duration_ms as f64 / 1000.0))
            .unwrap_or_else(|_| "unreadable".into());
        println!("saved {} ({length}) → {}", c.monitor, c.path.display());
    }
    Ok(())
}
