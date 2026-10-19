//! `scrawl` (no arguments) on Linux: a background daemon that owns recordings.
//!
//! `scrawl toggle` (what the Hyprland bind runs) talks to it over
//! `$XDG_RUNTIME_DIR/scrawl/daemon.sock` and starts it on demand, so no autostart entry or unit
//! is needed. On start it registers the record shortcut as a runtime Hyprland bind (no file is
//! edited; `hyprctl reload` drops it and the daemon adds it back).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::capture::{Mods, ScreenCaptureSession, SharedCaptureTelemetry, scrawl_log};
use crate::linux::{desktop, events, gsr, hypr};
use crate::model::TargetInfo;

pub fn socket_path() -> PathBuf {
    gsr::runtime_dir().join("daemon.sock")
}

/// The record shortcut, as a Hyprland key spec.
pub fn hotkey() -> Option<String> {
    let v = std::env::var("SCRAWL_HOTKEY").ok().or_else(|| {
        let raw = std::fs::read(crate::handoff::default_config_path()).ok()?;
        let cfg: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        cfg.get("linuxHotkey")?.as_str().map(str::to_owned)
    });
    match v.as_deref() {
        Some("" | "none" | "off") => None,
        Some(s) => Some(s.to_owned()),
        None => Some("ALT + SHIFT + S".to_owned()),
    }
}

fn exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "scrawl".into())
}

/// `ALT + SHIFT + S` → (modmask, key) as `hyprctl binds -j` reports them.
fn parse_spec(spec: &str) -> (u64, String) {
    let mut mask = 0;
    let mut key = String::new();
    for part in spec.split('+').map(str::trim) {
        match part.to_ascii_uppercase().as_str() {
            "SHIFT" => mask |= 1,
            "CTRL" | "CONTROL" => mask |= 4,
            "ALT" => mask |= 8,
            "SUPER" | "WIN" | "LOGO" | "MOD4" => mask |= 64,
            _ => key = part.to_owned(),
        }
    }
    (mask, key)
}

/// Adds the record shortcut as a runtime bind unless something else already owns that chord.
pub fn register_hotkey() {
    let Some(spec) = hotkey() else { return };
    let (mask, key) = parse_spec(&spec);
    if let Ok(binds) = hypr::json::<Vec<serde_json::Value>>("binds") {
        let taken = binds.iter().find(|b| {
            b["modmask"].as_u64() == Some(mask)
                && b["key"].as_str().is_some_and(|k| k.eq_ignore_ascii_case(&key))
                && !b["arg"].as_str().is_some_and(|a| a.contains("scrawl"))
                && b["dispatcher"].as_str() != Some("__lua")
        });
        if let Some(b) = taken {
            scrawl_log(format!(
                "{spec} is already bound ({} {}); not adding Scrawl's shortcut. Set linuxHotkey in ~/.config/scrawl/config.json or SCRAWL_HOTKEY.",
                b["dispatcher"], b["arg"]
            ));
            return;
        }
    }
    let cmd = format!("{} toggle", exe());
    let lua = format!(
        "if scrawl_hotkey == nil then scrawl_hotkey = hl.bind({}, hl.dsp.exec_cmd({}), {{ description = \"Scrawl: record / stop\" }}) end",
        hypr::lua_str(&spec),
        hypr::lua_str(&cmd)
    );
    match hypr::eval(&lua) {
        Ok(_) => scrawl_log(format!("shortcut {spec} → {cmd}")),
        Err(e) => scrawl_log(format!("could not add the shortcut: {e:#}")),
    }
}

struct Active {
    started: Instant,
    bundle_dir: PathBuf,
    target: TargetInfo,
    session: ScreenCaptureSession,
    source: events::Source,
    window_log: crate::windows::WindowLogger,
    overlay: Option<crate::linux::overlay::Overlay>,
    notice: Option<u32>,
}

enum State {
    Idle,
    Recording(Box<Active>),
    Finishing,
}

struct Daemon {
    state: Mutex<State>,
    telemetry: Arc<SharedCaptureTelemetry>,
    last_bundle: Mutex<Option<PathBuf>>,
}

pub fn captures_dir() -> PathBuf {
    crate::handoff::default_captures_dir()
}

/// Upstream defaults to `~/Library/Application Support/scrawl/captures`; on Linux the XDG data
/// dir, unless `SCRAWL_CAPTURES_DIR` says otherwise.
pub fn init_env() {
    if std::env::var_os("SCRAWL_CAPTURES_DIR").is_none() {
        let dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("scrawl/captures");
        // SAFETY: called first thing in `main`, before any thread starts.
        unsafe { std::env::set_var("SCRAWL_CAPTURES_DIR", dir) };
    }
}

fn current_mods() -> Mods {
    let lua = r#"local function d(k) local ok, r = pcall(hl.is_key_down, k); return (ok and r) and "1" or "0" end
return d("Shift_L")..d("Shift_R").." "..d("Control_L")..d("Control_R").." "..d("Alt_L")..d("Alt_R").." "..d("Super_L")..d("Super_R")"#;
    let r = hypr::eval(lua).unwrap_or_default();
    let parts: Vec<&str> = r.split_whitespace().collect();
    let on = |i: usize| parts.get(i).is_some_and(|p| p.contains('1'));
    Mods {
        shift: on(0),
        ctrl: on(1),
        alt: on(2),
        logo: on(3),
    }
}

impl Daemon {
    fn start(&self) -> Result<String> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            State::Recording(_) => return Ok("already recording".into()),
            State::Finishing => return Ok("still finishing the last capture".into()),
            State::Idle => {}
        }
        let target = crate::capture::discover_target(None);
        let dir = captures_dir().join(format!("{}.scrawl", crate::make_bundle_id()));
        std::fs::create_dir_all(&dir)?;
        let started = Instant::now();
        self.telemetry
            .reset_for_session(target.frame, started, current_mods());
        let source = events::Source::start(Arc::clone(&self.telemetry))
            .context("Could not hook input (hyprctl eval)")?;
        let session = ScreenCaptureSession::start_at(target.clone(), Some(dir.join("recording.mp4")), started);
        if !session.is_recording() {
            source.stop();
            self.telemetry.finish_session();
            let _ = std::fs::remove_dir_all(&dir);
            let msg = if gsr::other_running() {
                "Another screen recording is running; stop it first."
            } else {
                "gpu-screen-recorder failed to start; see ~/.local/state/scrawl/scrawl.log"
            };
            desktop::notify(desktop::Notice {
                title: "Scrawl couldn't record",
                body: msg,
                image: None,
                on_click: None,
                replaces: None,
                critical: true,
            });
            bail!("{msg}");
        }
        desktop::refresh_indicators();
        let window_log = crate::windows::WindowLogger::start(
            target.frame,
            started,
            Arc::clone(&self.telemetry.window_log_request),
        );
        let overlay = crate::linux::overlay::Overlay::start(Arc::clone(&self.telemetry), &target)
            .inspect_err(|e| scrawl_log(format!("overlay unavailable: {e:#}")))
            .ok();
        let key = hotkey().unwrap_or_else(|| "scrawl toggle".into());
        let notice = desktop::notify(desktop::Notice {
            title: "Scrawl is recording",
            body: &format!("Talk, click, hold Shift and move to draw. {key} to finish."),
            image: None,
            on_click: None,
            replaces: None,
            critical: false,
        });
        scrawl_log(format!("recording {} into {}", target_label(&target), dir.display()));
        *state = State::Recording(Box::new(Active {
            started,
            bundle_dir: dir,
            target,
            session,
            source,
            window_log,
            overlay,
            notice,
        }));
        Ok("recording".into())
    }

    /// Stops the recording and builds the bundle (on the calling thread).
    fn stop(self: &Arc<Self>, reason: &str) -> Result<String> {
        let active = {
            let mut state = self.state.lock().unwrap();
            match std::mem::replace(&mut *state, State::Finishing) {
                State::Recording(a) => {
                    // Like upstream: a press within 400 ms of the start is the same tap.
                    if a.started.elapsed() < Duration::from_millis(400) && reason == "toggle" {
                        *state = State::Recording(a);
                        return Ok("recording".into());
                    }
                    a
                }
                other => {
                    *state = other;
                    return Ok("not recording".into());
                }
            }
        };
        let result = self.finish(*active, reason);
        *self.state.lock().unwrap() = State::Idle;
        result
    }

    fn finish(&self, a: Active, reason: &str) -> Result<String> {
        let stop_started = Instant::now();
        a.source.stop();
        let (events, strokes) = self.telemetry.finish_session();
        if let Some(o) = a.overlay {
            o.stop();
        }
        let recording = a.session.stop();
        desktop::refresh_indicators();
        let mut activity = a.window_log.stop();
        activity.stop_started = Some(stop_started);
        activity.stop_ms = Some(stop_started.elapsed().as_millis() as u64);
        scrawl_log(format!(
            "stopped ({reason}): {} ms, video offset {} ms, {} events, {} strokes, {} candidates",
            recording.duration_ms,
            recording.video_offset_ms,
            events.len(),
            strokes.len(),
            recording.frames.len()
        ));
        let processing = desktop::notify(desktop::Notice {
            title: "Scrawl",
            body: "Transcribing and picking keyframes…",
            image: None,
            on_click: None,
            replaces: a.notice,
            critical: false,
        });
        let wav = a.bundle_dir.join("audio.wav");
        let recorded_audio = recording.video.as_deref().and_then(|v| {
            gsr::extract_audio(v, &wav, recording.video_offset_ms)
                .inspect_err(|e| scrawl_log(format!("audio extract: {e:#}")))
                .ok()?;
            crate::audio::recorded_audio_from_wav(std::fs::read(&wav).ok()?).ok()
        });
        let (dir, bundle, summary) = crate::finalize_capture_bundle(
            &a.bundle_dir,
            a.target,
            recording,
            strokes,
            events,
            recorded_audio,
            None,
            activity,
        )?;
        *self.last_bundle.lock().unwrap() = Some(dir.clone());
        let copied = std::env::var_os("SCRAWL_NO_CLIPBOARD").is_none() && desktop::copy_text(&summary).is_ok();
        let sheet = dir.join(&bundle.contact_sheet);
        desktop::notify(desktop::Notice {
            title: if copied { "Scrawl saved · copied" } else { "Scrawl saved" },
            body: &format!(
                "{} keyframes, {} s. {}",
                bundle.keyframes.len(),
                bundle.duration_ms / 1000,
                if copied { "Paste it into your agent." } else { "" }
            ),
            image: sheet.is_file().then_some(sheet.as_path()),
            on_click: Some(vec!["xdg-open".into(), dir.display().to_string()]),
            replaces: processing,
            critical: false,
        });
        scrawl_log(format!("saved {}", dir.display()));
        Ok(dir.display().to_string())
    }

    fn status(&self) -> String {
        match &*self.state.lock().unwrap() {
            State::Idle => "idle".into(),
            State::Recording(a) => format!("recording {} s", a.started.elapsed().as_secs()),
            State::Finishing => "finishing".into(),
        }
    }
}

fn target_label(t: &TargetInfo) -> String {
    format!(
        "{:?} {}x{}+{}+{} @{}",
        t.mode, t.frame.width, t.frame.height, t.frame.x, t.frame.y, t.frame.scale
    )
}

/// Runs the daemon until `quit`.
pub fn run(record_now: bool) -> Result<()> {
    let dir = gsr::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::File::create(dir.join("daemon.lock"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another scrawl daemon is running");
    }
    hypr::signature().context("Scrawl on Linux needs a Hyprland session")?;
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    let (tx, _rx) = flume::unbounded();
    let daemon = Arc::new(Daemon {
        state: Mutex::new(State::Idle),
        telemetry: SharedCaptureTelemetry::new(tx),
        last_bundle: Mutex::new(None),
    });
    register_hotkey();
    scrawl_log(format!("scrawl daemon {} pid {}", env!("CARGO_PKG_VERSION"), std::process::id()));

    // Re-add the shortcut after `hyprctl reload`; notice a recording stopped from outside
    // (Omarchy's screen-record toggle SIGINTs every gpu-screen-recorder).
    {
        std::thread::spawn(|| loop {
            let _ = hypr::events(|name, _| {
                if name == "configreloaded" {
                    register_hotkey();
                }
                true
            });
            std::thread::sleep(Duration::from_secs(2));
            hypr::reset();
        });
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let stopped_outside = matches!(&*d.state.lock().unwrap(), State::Recording(a) if !a.session.is_recording());
            if stopped_outside {
                scrawl_log("gpu-screen-recorder stopped from outside; finishing the capture");
                let _ = d.stop("stopped outside");
            }
        });
    }
    if record_now {
        let _ = daemon.start();
    }
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || {
            let _ = serve(&d, conn);
        });
        if !sock.exists() {
            break;
        }
    }
    Ok(())
}

fn serve(d: &Arc<Daemon>, conn: UnixStream) -> Result<()> {
    let mut line = String::new();
    BufReader::new(conn.try_clone()?).read_line(&mut line)?;
    let cmd = line.trim();
    let reply = match cmd {
        "toggle" => {
            let recording = matches!(&*d.state.lock().unwrap(), State::Recording(_));
            if recording { d.stop("toggle") } else { d.start() }
        }
        "start" => d.start(),
        "stop" => d.stop("stop"),
        "status" => Ok(d.status()),
        "last" => Ok(d
            .last_bundle
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()),
        "quit" => {
            let _ = d.stop("quit");
            let _ = std::fs::remove_file(socket_path());
            let mut c = &conn;
            let _ = writeln!(c, "ok bye");
            std::process::exit(0);
        }
        other => Err(anyhow::anyhow!("unknown command {other:?}")),
    };
    let mut c = &conn;
    match reply {
        Ok(r) => writeln!(c, "ok {r}")?,
        Err(e) => writeln!(c, "error {e:#}")?,
    }
    Ok(())
}

/// Sends one command to the daemon, starting it first if needed.
pub fn send(cmd: &str, spawn: bool) -> Result<String> {
    let sock = socket_path();
    let connect = || UnixStream::connect(&sock);
    let mut stream = match connect() {
        Ok(s) => s,
        Err(_) if spawn => {
            spawn_daemon()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(s) = connect() {
                    break s;
                }
                if Instant::now() > deadline {
                    bail!("the scrawl daemon did not start; see ~/.local/state/scrawl/scrawl.log");
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        Err(_) => bail!("the scrawl daemon is not running"),
    };
    // Stopping waits for transcription; give it time.
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    writeln!(stream, "{cmd}")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply = reply.trim();
    if let Some(r) = reply.strip_prefix("ok ") {
        Ok(r.to_owned())
    } else {
        bail!("{}", reply.strip_prefix("error ").unwrap_or(reply))
    }
}

fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::capture::log_dir().join("daemon.out"))
        .or_else(|_| {
            std::fs::create_dir_all(crate::capture::log_dir())?;
            std::fs::File::create(crate::capture::log_dir().join("daemon.out"))
        })?;
    let mut cmd = std::process::Command::new(exe());
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("Could not start the scrawl daemon")?;
    Ok(())
}

pub fn bundle_dir_hint(p: &Path) -> String {
    p.display().to_string()
}
