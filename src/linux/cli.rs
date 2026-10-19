//! Linux-only subcommands, handled before upstream's clap parser so `main.rs` stays close to
//! upstream: `toggle`, `start`, `stop`, `status`, `daemon`, `quit`, `last`, `paste`, `doctor`,
//! `hyprland-snippet`, `setup transcription`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::linux::{daemon, desktop, gsr, hypr, whisper};

const USAGE: &str = "Linux commands:
  scrawl toggle               start or finish a recording (bind this to a key)
  scrawl start | stop         explicit start / finish
  scrawl status               idle, recording or finishing
  scrawl paste [BUNDLE]       paste the contact sheet + summary of the latest (or given) capture
  scrawl doctor               check what this machine has
  scrawl hyprland-snippet     print a persistent bind for ~/.config/hypr/bindings.lua
  scrawl setup transcription  download a whisper model (asks first)
  scrawl daemon               run the background daemon in the foreground
  scrawl quit                 stop the daemon";

/// Returns `Some(result)` when the arguments were a Linux command.
pub fn intercept() -> Option<Result<()>> {
    daemon::init_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(String::as_str)?;
    Some(match first {
        "toggle" | "start" | "stop" => daemon::send(first, first != "stop").map(|r| println!("{r}")),
        "status" | "last" => match daemon::send(first, false) {
            Ok(r) => {
                println!("{r}");
                Ok(())
            }
            Err(_) if first == "status" => {
                println!("not running");
                Ok(())
            }
            Err(e) => Err(e),
        },
        "quit" => daemon::send("quit", false).map(|_| ()),
        "daemon" => daemon::run(args.iter().any(|a| a == "--record")),
        "paste" => paste(args.get(1).map(PathBuf::from)),
        "doctor" => doctor(),
        "hyprland-snippet" => {
            let key = daemon::hotkey().unwrap_or_else(|| "ALT + SHIFT + S".into());
            println!("-- ~/.config/hypr/bindings.lua");
            println!("o.bind(\"{key}\", \"Scrawl: record / stop\", \"scrawl toggle\")");
            println!("o.bind(\"ALT + SHIFT + V\", \"Scrawl: paste latest\", \"scrawl paste\")");
            Ok(())
        }
        "setup" if args.get(1).map(String::as_str) == Some("transcription") => setup_transcription(),
        "debug-events" => debug_events(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5)),
        "debug-overlay" => debug_overlay(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4)),
        "help-linux" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => return None,
    })
}

fn paste(bundle: Option<PathBuf>) -> Result<()> {
    let dir = match bundle {
        Some(b) => crate::handoff::resolve_bundle_dir(&b)?,
        None => crate::handoff::resolve_bundle_dir(std::path::Path::new("latest"))?,
    };
    desktop::paste_bundle(&dir)
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(cmd)).find(|p| p.is_file())
}

fn doctor() -> Result<()> {
    let mut bad = 0;
    let mut row = |ok: bool, what: &str, detail: String| {
        if !ok {
            bad += 1;
        }
        println!("{} {what:<22} {detail}", if ok { "✓" } else { "✗" });
    };
    match hypr::signature() {
        Ok(sig) => {
            let v = hypr::request("version").unwrap_or_default();
            let ver = v.lines().next().unwrap_or("").to_owned();
            row(true, "Hyprland", format!("{ver} ({})", &sig[sig.len().saturating_sub(24)..]));
            let lua = hypr::eval("return 1").is_ok();
            row(lua, "Lua eval (≥ 0.55)", if lua { "ok".into() } else { "hyprctl eval failed: Hyprland ≥ 0.55 with a Lua config is required".into() });
        }
        Err(e) => row(false, "Hyprland", format!("{e:#}")),
    }
    for (cmd, why, pkg) in [
        ("gpu-screen-recorder", "screen + mic recording", "gpu-screen-recorder"),
        ("ffmpeg", "frames and audio", "ffmpeg"),
        ("ffprobe", "media probing", "ffmpeg"),
        ("wl-copy", "clipboard", "wl-clipboard"),
        ("wtype", "paste", "wtype"),
    ] {
        let found = which(cmd);
        row(found.is_some(), cmd, found.map_or(format!("missing ({why}): omarchy-pkg-add {pkg}"), |p| p.display().to_string()));
    }
    let gsr_ver = std::process::Command::new("gpu-screen-recorder").arg("--version").output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
    if !gsr_ver.is_empty() {
        println!("  gpu-screen-recorder {gsr_ver} (needs -write-first-frame-ts; 6.1.0 has it)");
    }
    if gsr::other_running() {
        row(false, "recorder free", "a gpu-screen-recorder is running now (Omarchy recording?)".into());
    }
    let cli = whisper::cli();
    row(cli.is_some(), "whisper-cli", cli.map_or("missing: omarchy-pkg-add whisper-cpp (or set SCRAWL_WHISPER_CLI)".into(), |p| p.display().to_string()));
    let model = whisper::model();
    row(model.is_some(), "whisper model", model.map_or("missing: scrawl setup transcription".into(), |p| p.display().to_string()));
    match daemon::hotkey() {
        Some(k) => println!("  shortcut: {k} (runtime bind; `scrawl hyprland-snippet` for a permanent one)"),
        None => println!("  shortcut: none"),
    }
    println!("  captures: {}", daemon::captures_dir().display());
    println!("  log:      {}", crate::capture::log_dir().join("scrawl.log").display());
    if bad > 0 {
        bail!("{bad} problem(s)");
    }
    Ok(())
}

fn setup_transcription() -> Result<()> {
    use std::io::Write;
    let dir = dirs::data_dir().context("no data dir")?.join("scrawl/models");
    let file = dir.join("ggml-base.en.bin");
    if let Some(m) = whisper::model() {
        println!("Already have a model: {}", m.display());
        return Ok(());
    }
    let url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";
    print!("Download whisper base.en (148 MB) from huggingface.co into {}? [y/N] ", dir.display());
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        bail!("cancelled");
    }
    std::fs::create_dir_all(&dir)?;
    let part = file.with_extension("bin.part");
    let status = std::process::Command::new("curl")
        .args(["-fL", "--progress-bar", "-o"])
        .arg(&part)
        .arg(url)
        .status()
        .context("Could not run curl")?;
    if !status.success() {
        bail!("download failed");
    }
    std::fs::rename(&part, &file)?;
    println!("Saved {}", file.display());
    if whisper::cli().is_none() {
        println!("Now install whisper-cli: omarchy-pkg-add whisper-cpp");
    }
    Ok(())
}

/// Runs only the input source for N seconds against the full screen and prints what it saw:
/// for testing the Lua hook (e.g. in a nested Hyprland with `SCRAWL_HYPR_SIG`).
fn debug_events(seconds: u64) -> Result<()> {
    let target = crate::capture::discover_target(None);
    let (tx, _rx) = flume::unbounded();
    let telemetry = crate::capture::SharedCaptureTelemetry::new(tx);
    let started = std::time::Instant::now();
    telemetry.reset_for_session(target.frame, started, crate::capture::Mods::default());
    let source = crate::linux::events::Source::start(std::sync::Arc::clone(&telemetry))?;
    eprintln!("listening {seconds} s on {:?}", target.frame);
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    source.stop();
    let (events, strokes) = telemetry.finish_session();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "events": events, "strokes": strokes }))?);
    Ok(())
}

/// Shows the overlay (start glow + a synthetic Shift-ink stroke) for N seconds.
fn debug_overlay(seconds: u64) -> Result<()> {
    let target = crate::capture::discover_target(None);
    let (tx, _rx) = flume::unbounded();
    let t = crate::capture::SharedCaptureTelemetry::new(tx);
    t.reset_for_session(target.frame, std::time::Instant::now(), crate::capture::Mods::default());
    t.set_mods(crate::capture::Mods { shift: true, ..Default::default() });
    let f = target.frame;
    for i in 0..60 {
        let a = i as f64 / 59.0 * std::f64::consts::TAU;
        t.cursor_sample(i * 16, f.x + f.width * (0.5 + 0.15 * a.cos()), f.y + f.height * (0.5 + 0.2 * a.sin()));
    }
    let overlay = crate::linux::overlay::Overlay::start(std::sync::Arc::clone(&t), &target)?;
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    overlay.stop();
    Ok(())
}
