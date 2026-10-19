//! `scrawl` (no arguments) on Linux: a background daemon that owns recordings.
//!
//! `scrawl toggle` (what the Hyprland bind runs) talks to it over
//! `$XDG_RUNTIME_DIR/scrawl/daemon.sock` and starts it on demand, so no autostart entry or unit
//! is needed. On start it registers the record shortcut as a runtime Hyprland bind (no file is
//! edited; `hyprctl reload` drops it and the daemon adds it back).

use std::io::{BufRead, BufReader, Read, Write};
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

/// A shortcut as a Hyprland key spec: `env`, else `key` in the config, else `default`;
/// "none" turns it off.
fn shortcut(env: &str, key: &str, default: &str) -> Option<String> {
    let v = std::env::var(env).ok().or_else(|| {
        let raw = std::fs::read(crate::handoff::default_config_path()).ok()?;
        let cfg: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        cfg.get(key)?.as_str().map(str::to_owned)
    });
    match v.as_deref() {
        Some("" | "none" | "off") => None,
        Some(s) => Some(s.to_owned()),
        None => Some(default.to_owned()),
    }
}

/// Record / stop (upstream's ⌥⇧S).
pub fn hotkey() -> Option<String> {
    shortcut("SCRAWL_HOTKEY", "linuxHotkey", "ALT + SHIFT + S")
}

/// Paste the latest capture (upstream's ⌥⇧V).
pub fn paste_hotkey() -> Option<String> {
    shortcut("SCRAWL_PASTE_HOTKEY", "linuxPasteHotkey", "ALT + SHIFT + V")
}

/// Open the latest capture in the review window (upstream's Inspect chip and Open Latest Capture).
pub fn review_hotkey() -> Option<String> {
    shortcut("SCRAWL_REVIEW_HOTKEY", "linuxReviewHotkey", "ALT + SHIFT + R")
}

/// Whether a finished capture opens its review window by itself (`"linuxReviewOnFinish": true`).
pub fn review_on_finish() -> bool {
    std::env::var("SCRAWL_REVIEW_ON_FINISH").ok().map_or_else(
        || {
            std::fs::read(crate::handoff::default_config_path())
                .ok()
                .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
                .and_then(|cfg| cfg.get("linuxReviewOnFinish")?.as_bool())
                .unwrap_or(false)
        },
        |v| v == "1" || v.eq_ignore_ascii_case("true"),
    )
}

/// "ALT + SHIFT + R" → "Alt+Shift+R".
pub fn pretty_key(spec: &str) -> String {
    spec.split('+')
        .map(|part| {
            let part = part.trim();
            match part.to_ascii_uppercase().as_str() {
                "ALT" => "Alt".into(),
                "SHIFT" => "Shift".into(),
                "CTRL" | "CONTROL" => "Ctrl".into(),
                "SUPER" | "MOD4" | "WIN" => "Super".into(),
                _ => part.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Opens `dir` in the review window as its own process.
fn open_review(dir: &Path) {
    match std::process::Command::new(exe())
        .arg("preview")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // Reap it when it closes.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => scrawl_log(format!("could not open the review window: {e:#}")),
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

/// The review window floats like upstream's panel instead of being tiled. A runtime rule, like the
/// shortcuts: nothing is written to the user's config, and a reload drops it until it is added back.
fn register_window_rule() {
    let lua = r#"if scrawl_review_rule == nil then scrawl_review_rule = hl.window_rule({ name = "scrawl-review", match = { class = "dev.scrawl.Scrawl" }, float = true, center = true, size = "1120 740" }) end"#;
    if let Err(e) = hypr::eval(lua) {
        scrawl_log(format!("could not add the review window rule: {e:#}"));
    }
}

/// Adds Scrawl's shortcuts as runtime binds, each unless something else already owns its chord.
pub fn register_hotkey() {
    register_window_rule();
    let binds = hypr::json::<Vec<serde_json::Value>>("binds").unwrap_or_default();
    let exe = exe();
    for (global, spec, command, description, setting) in [
        ("scrawl_hotkey", hotkey(), "toggle", "Scrawl: record / stop", "linuxHotkey"),
        ("scrawl_paste_hotkey", paste_hotkey(), "paste", "Scrawl: paste the latest capture", "linuxPasteHotkey"),
        ("scrawl_review_hotkey", review_hotkey(), "preview latest", "Scrawl: review the latest capture", "linuxReviewHotkey"),
    ] {
        let Some(spec) = spec else { continue };
        let (mask, key) = parse_spec(&spec);
        // Omarchy's binds are Lua too, so tell ours apart by description.
        let taken = binds.iter().find(|b| {
            b["modmask"].as_u64() == Some(mask)
                && b["key"].as_str().is_some_and(|k| k.eq_ignore_ascii_case(&key))
                && b["submap"].as_str().unwrap_or("").is_empty()
                && !b["description"].as_str().is_some_and(|d| d.starts_with("Scrawl"))
        });
        if let Some(b) = taken {
            scrawl_log(format!(
                "{spec} is already bound ({}); not adding \"{description}\". Set {setting} in ~/.config/scrawl/config.json.",
                b["description"].as_str().unwrap_or("?")
            ));
            continue;
        }
        let cmd = format!("{exe} {command}");
        let lua = format!(
            "if {global} == nil then {global} = hl.bind({}, hl.dsp.exec_cmd({}), {{ description = {} }}) end",
            hypr::lua_str(&spec),
            hypr::lua_str(&cmd),
            hypr::lua_str(description)
        );
        match hypr::eval(&lua) {
            Ok(_) => scrawl_log(format!("shortcut {spec} → {cmd}")),
            Err(e) => scrawl_log(format!("could not add {spec}: {e:#}")),
        }
    }
}

struct Active {
    /// Which recording this is, so a late watcher can't stop the next one.
    id: u64,
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
    /// `slurp` is up for an area or window; `slurp` is a pidfd for it once it has started, so a
    /// cancel can't signal a reused pid after the picker was reaped.
    Picking { id: u64, slurp: Option<std::os::fd::OwnedFd> },
    Recording(Box<Active>),
    Finishing,
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct Daemon {
    state: Mutex<State>,
    /// Set once shutdown starts: no new recording may begin.
    closing: std::sync::atomic::AtomicBool,
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
        self.start_on(None, None)
    }

    /// Starts recording `target` (the display under the cursor if `None`). `pick` is the pick this
    /// target came from: it starts only if that pick is still the current state (not cancelled).
    fn start_on(&self, target: Option<TargetInfo>, pick: Option<u64>) -> Result<String> {
        let mut state = self.state.lock().unwrap();
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok("shutting down".into());
        }
        match (&*state, pick) {
            (State::Idle, None) => {}
            (State::Picking { id, .. }, Some(pick)) if *id == pick => *state = State::Idle,
            (_, Some(_)) => return Ok("cancelled".into()),
            (State::Picking { .. }, None) => return Ok("picking an area or window".into()),
            (State::Recording(_), None) => return Ok("already recording".into()),
            (State::Finishing, None) => return Ok("still finishing the last capture".into()),
        }
        let target = target.unwrap_or_else(|| crate::capture::discover_target(None));
        let dir = captures_dir().join(format!("{}.scrawl", crate::make_bundle_id()));
        std::fs::create_dir_all(&dir)?;
        let started = Instant::now();
        self.telemetry
            .reset_for_session(target.frame, started, current_mods());
        let source = match events::Source::start(Arc::clone(&self.telemetry)) {
            Ok(source) => source,
            Err(e) => {
                self.telemetry.finish_session();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e.context("Could not hook input (hyprctl eval)"));
            }
        };
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
        let key = hotkey().map_or_else(|| "scrawl toggle".into(), |k| pretty_key(&k));
        let notice = desktop::notify(desktop::Notice {
            title: "Scrawl is recording",
            body: &format!("Talk, click, hold Shift and move to draw (Ctrl+Z undoes a stroke). {key} to finish, Esc to cancel."),
            image: None,
            on_click: None,
            replaces: None,
            critical: false,
        });
        scrawl_log(format!("recording {} into {}", target_label(&target), dir.display()));
        *state = State::Recording(Box::new(Active {
            id: next_id(),
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

    /// Stops the recording and builds the bundle (on the calling thread). `asked` is when the
    /// request arrived (the start holds the state lock while the recorder comes up, so a press
    /// can wait here past the tap window). `only` stops nothing unless that recording is current.
    fn stop(self: &Arc<Self>, reason: &str, asked: Instant, only: Option<u64>) -> Result<String> {
        let active = {
            let mut state = self.state.lock().unwrap();
            match std::mem::replace(&mut *state, State::Finishing) {
                State::Recording(a) if only.is_some_and(|id| id != a.id) => {
                    *state = State::Recording(a);
                    return Ok("not recording".into());
                }
                State::Recording(a) => {
                    // Like upstream: a press within 400 ms of the start is the same tap.
                    if reason == "toggle" && asked.saturating_duration_since(a.started) < Duration::from_millis(400) {
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

    /// Esc while recording, like upstream: stop everything and keep nothing. Also closes a picker.
    fn cancel(&self) -> Result<String> {
        let a = {
            let mut state = self.state.lock().unwrap();
            match std::mem::replace(&mut *state, State::Finishing) {
                State::Recording(a) => a,
                State::Picking { slurp, .. } => {
                    *state = State::Idle;
                    if let Some(fd) = slurp {
                        signal_pidfd(&fd, libc::SIGTERM);
                    }
                    return Ok("cancelled".into());
                }
                other => {
                    *state = other;
                    return Ok("not recording".into());
                }
            }
        };
        a.source.stop();
        let _ = self.telemetry.finish_session();
        if let Some(o) = a.overlay {
            o.stop();
        }
        a.session.discard();
        let _ = a.window_log.stop();
        desktop::refresh_indicators();
        let _ = std::fs::remove_dir_all(&a.bundle_dir);
        scrawl_log("cancelled: recording discarded");
        desktop::notify(desktop::Notice {
            title: "Scrawl cancelled",
            body: "Esc stopped the recording. Nothing was saved.",
            image: None,
            on_click: None,
            replaces: a.notice,
            critical: false,
        });
        *self.state.lock().unwrap() = State::Idle;
        Ok("cancelled".into())
    }

    fn finish(&self, a: Active, reason: &str) -> Result<String> {
        let stop_started = Instant::now();
        a.source.stop();
        let (events, strokes) = self.telemetry.finish_session();
        if let Some(o) = a.overlay {
            o.stop();
        }
        // The audio goes out to be transcribed while the candidate frames decode.
        let wav = a.bundle_dir.join("audio.wav");
        let mut recorded_audio = None;
        let recording = a.session.stop_with(|video, offset_ms| {
            desktop::refresh_indicators();
            recorded_audio = video.and_then(|v| {
                gsr::extract_audio(v, &wav, offset_ms)
                    .inspect_err(|e| scrawl_log(format!("audio extract: {e:#}")))
                    .ok()?;
                crate::audio::recorded_audio_from_wav(std::fs::read(&wav).ok()?).ok()
            });
            if recorded_audio.as_ref().is_some_and(|audio| audio.sample_count > 0)
                && let Err(e) = crate::helper::prefetch(&wav)
            {
                scrawl_log(format!("transcription prefetch: {e:#}"));
            }
        });
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
            title: if copied { "Scrawl saved · summary copied" } else { "Scrawl saved" },
            body: &format!(
                "{} keyframes, {} s. Click to review{}{}.",
                bundle.keyframes.len(),
                bundle.duration_ms / 1000,
                review_hotkey().map_or(String::new(), |k| format!(" ({} any time)", pretty_key(&k))),
                paste_hotkey().map_or(String::new(), |k| format!(" · {} pastes sheet + summary", pretty_key(&k))),
            ),
            image: sheet.is_file().then_some(sheet.as_path()),
            on_click: Some(vec![exe(), "preview".into(), dir.display().to_string()]),
            replaces: processing,
            critical: false,
        });
        if review_on_finish() {
            open_review(&dir);
        }
        scrawl_log(format!("saved {}", dir.display()));
        Ok(dir.display().to_string())
    }

    /// Ends the daemon without losing work: a recording is finished (saved), a capture that is
    /// already finishing is waited for. Then the input hook is switched off and the process exits.
    fn shutdown(self: &Arc<Self>, reason: &str, before_exit: impl FnOnce()) -> ! {
        // Set before looking at the state; start_on checks it under the state lock, so a start
        // either completes before this loop sees it (and is finished below) or never begins.
        self.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        loop {
            let (finishing, picking) = match &*self.state.lock().unwrap() {
                State::Idle => break,
                State::Picking { .. } => (false, true),
                State::Recording(_) => (false, false),
                State::Finishing => (true, false),
            };
            if finishing {
                std::thread::sleep(Duration::from_millis(100));
            } else if picking {
                let _ = self.cancel();
            } else if let Err(e) = self.stop(reason, Instant::now(), None) {
                scrawl_log(format!("finishing on {reason}: {e:#}"));
            }
        }
        events::close();
        let _ = std::fs::remove_file(socket_path());
        scrawl_log(format!("daemon exiting ({reason})"));
        before_exit();
        std::process::exit(0);
    }

    fn status(&self) -> String {
        match &*self.state.lock().unwrap() {
            State::Idle => "idle".into(),
            State::Picking { .. } => "picking".into(),
            State::Recording(a) => format!("recording {} s", a.started.elapsed().as_secs()),
            State::Finishing => "finishing".into(),
        }
    }
}

impl Daemon {
    /// Puts up the picker and records what is chosen. Only from Idle; `cancel` (Esc, `scrawl
    /// cancel`) and shutdown close the picker, and a pick that was cancelled never starts.
    fn pick(&self) -> Result<String> {
        let id = next_id();
        {
            let mut state = self.state.lock().unwrap();
            if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok("shutting down".into());
            }
            match &*state {
                State::Idle => *state = State::Picking { id, slurp: None },
                State::Picking { .. } => return Ok("already picking".into()),
                State::Recording(_) => return Ok("already recording".into()),
                State::Finishing => return Ok("still finishing the last capture".into()),
            }
        }
        let picked = pick_target(|pid| match &mut *self.state.lock().unwrap() {
            State::Picking { id: current, slurp } if *current == id => {
                // Opened before anything can reap the child, so it names this slurp for good.
                *slurp = pidfd_open(pid);
                true
            }
            _ => false,
        });
        match picked {
            Some(target) => self.start_on(Some(target), Some(id)),
            None => {
                let mut state = self.state.lock().unwrap();
                if matches!(&*state, State::Picking { id: current, .. } if *current == id) {
                    *state = State::Idle;
                }
                Ok("cancelled".into())
            }
        }
    }
}

/// Upstream's tap rule: the shortcut's modifiers come up within 250 ms. Polled through the
/// compositor, since the shortcut fires on key down.
fn held_past_tap() -> bool {
    let lua = r#"local function d(k) local ok, r = pcall(hl.is_key_down, k); return ok and r end
return (d("Shift_L") or d("Shift_R") or d("Control_L") or d("Control_R") or d("Alt_L") or d("Alt_R") or d("Super_L") or d("Super_R")) and "held" or "free""#;
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        match hypr::eval(lua) {
            Ok(r) if r.contains("held") => std::thread::sleep(Duration::from_millis(15)),
            _ => return false,
        }
    }
    true
}

/// `slurp` over the visible windows: drag for an area, click a window for that window, Esc
/// cancels. (Omarchy's own region capture uses slurp too.) `started(pid)` hands over the picker's
/// pid; when it says no (the pick was cancelled meanwhile) the picker is closed at once.
fn pick_target(started: impl FnOnce(u32) -> bool) -> Option<TargetInfo> {
    let windows = hypr::visible_clients().unwrap_or_default();
    let boxes: String = windows
        .iter()
        .map(|c| {
            format!(
                "{},{} {}x{} {}\n",
                c.at[0] as i64, c.at[1] as i64, c.size[0] as i64, c.size[1] as i64,
                c.class.replace(' ', "_")
            )
        })
        .collect();
    let mut child = std::process::Command::new("slurp")
        .args(["-f", "%x %y %w %h %l", "-b", "#00000055", "-c", "#ff3b30", "-s", "#ff3b3022", "-w", "2"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .inspect_err(|e| scrawl_log(format!("slurp: {e}")))
        .ok()?;
    // From here every path reaps the child.
    let wanted = started(child.id());
    let written = wanted
        && child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(boxes.as_bytes()).is_ok());
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut stdout);
    }
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&stdout);
    let f: Vec<&str> = text.split_whitespace().collect();
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
    let (x, y, w, h) = (num(0)?, num(1)?, num(2)?, num(3)?);
    if w < 16.0 || h < 16.0 {
        return None;
    }
    let label = f.get(4).copied().unwrap_or("");
    let picked_window = windows.iter().find(|c| {
        (c.at[0] - x).abs() < 1.0 && (c.at[1] - y).abs() < 1.0 && (c.size[0] - w).abs() < 1.0 && (c.size[1] - h).abs() < 1.0
            && (label.is_empty() || c.class.replace(' ', "_") == label)
    });
    Some(match picked_window {
        Some(c) => crate::capture::discover_target_window(c),
        None => crate::capture::region_target(x, y, w, h),
    })
}

fn pidfd_open(pid: u32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
}

/// Signals the process behind a pidfd; nothing happens if it has already gone.
fn signal_pidfd(fd: &std::os::fd::OwnedFd, sig: libc::c_int) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0);
    }
}

fn target_label(t: &TargetInfo) -> String {
    format!(
        "{:?} {}x{}+{}+{} @{}",
        t.mode, t.frame.width, t.frame.height, t.frame.x, t.frame.y, t.frame.scale
    )
}

/// Runs the daemon until `quit`.
/// The termination signal received, if any (set by `on_termination`).
static TERMINATION: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_termination(sig: libc::c_int) {
    // Async-signal-safe: only an atomic store.
    TERMINATION.store(sig, std::sync::atomic::Ordering::SeqCst);
}

/// SIGTERM / SIGINT / SIGHUP (a service stop or restart, Ctrl+C) end the daemon the way `quit`
/// does: a recording is finished and saved, a finishing one completes, then the daemon exits. A
/// handler records the signal; `watch_termination` acts on it. The signals are caught, not
/// blocked: a blocked mask is inherited by every child, and gpu-screen-recorder, ffmpeg and
/// whisper-cli must still stop when signalled. (Caught handlers reset to the default on exec.)
fn handle_termination() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_termination as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut action.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

fn watch_termination(daemon: Arc<Daemon>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        let sig = TERMINATION.load(std::sync::atomic::Ordering::SeqCst);
        if sig != 0 {
            scrawl_log(format!("signal {sig}: finishing up and exiting"));
            daemon.shutdown("signal", || {});
        }
    });
}

pub fn run(record_now: bool) -> Result<()> {
    let dir = gsr::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::File::create(dir.join("daemon.lock"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another scrawl daemon is running");
    }
    hypr::signature().context("Scrawl on Linux needs a Hyprland session")?;
    // A daemon that died while recording (crash, kill, service restart) left its input hook in
    // Hyprland, including the bind that consumes Esc. This daemon owns the hook now: switch it off.
    events::disable();
    // Its recorder may have been killed with it; let Omarchy's bar stop showing a recording.
    desktop::refresh_indicators();
    handle_termination();
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    let (tx, rx) = flume::unbounded();
    let daemon = Arc::new(Daemon {
        state: Mutex::new(State::Idle),
        closing: std::sync::atomic::AtomicBool::new(false),
        telemetry: SharedCaptureTelemetry::new(tx),
        last_bundle: Mutex::new(None),
    });
    watch_termination(Arc::clone(&daemon));
    {
        // Esc and Ctrl+Z from the input source.
        let d = Arc::clone(&daemon);
        std::thread::Builder::new().name("scrawl-ui-events".into()).spawn(move || {
            for event in rx.iter() {
                match event {
                    crate::capture::LiveOverlayEvent::CancelCapture => {
                        let d = Arc::clone(&d);
                        // Not on this thread: stopping the input source waits for its threads.
                        std::thread::spawn(move || d.cancel());
                    }
                    crate::capture::LiveOverlayEvent::UndoStroke => d.telemetry.undo_stroke(),
                    _ => {}
                }
            }
        })?;
    }
    register_hotkey();
    // Upstream's cleanup: heavy media to the Trash after `mediaDays`, bundles after `bundleDays`.
    crate::retention::start();
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
            let stopped_outside = match &*d.state.lock().unwrap() {
                State::Recording(a) if !a.session.is_recording() => Some(a.id),
                _ => None,
            };
            if let Some(id) = stopped_outside {
                scrawl_log("gpu-screen-recorder stopped from outside; finishing the capture");
                let _ = d.stop("stopped outside", Instant::now(), Some(id));
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
    let asked = Instant::now();
    let cmd = line.trim();
    let reply = match cmd {
        "toggle" => {
            let recording = matches!(&*d.state.lock().unwrap(), State::Recording(_));
            if recording {
                d.stop("toggle", asked, None)
            } else if held_past_tap() {
                // Held like upstream's switcher: pick an area or a window.
                d.pick()
            } else {
                d.start()
            }
        }
        "pick" => d.pick(),
        "start" => d.start(),
        "stop" => d.stop("stop", asked, None),
        "cancel" => d.cancel(),
        "status" => Ok(d.status()),
        "last" => Ok(d
            .last_bundle
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()),
        "quit" => {
            // Waits for a capture in progress (recording or finishing), replies, then exits.
            if !matches!(&*d.state.lock().unwrap(), State::Idle) {
                scrawl_log("quit: finishing the capture in progress first");
            }
            d.shutdown("quit", || {
                let mut c = &conn;
                let _ = writeln!(c, "ok bye");
            })
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
            // With the service installed, systemd owns the daemon: a daemon started here would
            // hold the lock and keep the service from ever starting again.
            let service = service_installed();
            if service {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "start", "scrawl.service"])
                    .status();
            } else {
                spawn_daemon()?;
            }
            let deadline = Instant::now() + Duration::from_secs(20);
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
    if reply.is_empty() {
        // `quit` (or a stop signal) ended the daemon while this command waited, e.g. a picker.
        bail!("the scrawl daemon exited before answering");
    }
    if let Some(r) = reply.strip_prefix("ok ") {
        Ok(r.to_owned())
    } else {
        bail!("{}", reply.strip_prefix("error ").unwrap_or(reply))
    }
}

/// Whether `scrawl setup service` installed the systemd user service.
fn service_installed() -> bool {
    dirs::config_dir().is_some_and(|d| d.join("systemd/user/scrawl.service").is_file())
}

/// Starts a daemon on demand (no service installed) as a transient systemd user unit, so it has
/// the service's guarantees: a crash restarts it (and the new daemon switches off the dead one's
/// hook and Esc bind), and the unit's cgroup takes a recorder it left behind with it. Without
/// systemd at all it falls back to a plain `setsid` daemon, which has no crash cleanup.
fn spawn_daemon() -> Result<()> {
    let mut run = std::process::Command::new("systemd-run");
    run.args(["--user", "--quiet", "--collect", "--unit=scrawl-ondemand"]);
    for property in crate::linux::cli::UNIT_PROPERTIES {
        run.args(["-p", property]);
    }
    // The user manager may not have this session's variables (no uwsm), so hand them over. The
    // unit doesn't run in the caller's directory: make relative paths absolute first.
    let cwd = std::env::current_dir().ok();
    for (key, mut value) in std::env::vars() {
        if PATH_VARS.contains(&key.as_str())
            && let Some(cwd) = &cwd
            && std::path::Path::new(&value).is_relative()
        {
            value = cwd.join(&value).display().to_string();
        }
        if key.starts_with("SCRAWL_")
            || matches!(
                key.as_str(),
                "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE" | "XDG_RUNTIME_DIR" | "DBUS_SESSION_BUS_ADDRESS" | "OMARCHY_PATH" | "PATH" | "CODEX_HOME"
            )
        {
            run.arg(format!("--setenv={key}={value}"));
        }
    }
    let ran = run
        .arg(exe())
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match ran {
        Ok(out) if out.status.success() => Ok(()),
        // Another command started the unit first (two at once, before the socket existed): wait
        // for that daemon rather than starting a competitor for the lock.
        Ok(_) if ondemand_unit_running() => Ok(()),
        Ok(out) => bail!(
            "could not start the scrawl daemon unit: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            scrawl_log("no systemd-run: starting the daemon without a supervisor");
            spawn_detached()
        }
        Err(e) => Err(e).context("running systemd-run"),
    }
}

/// `SCRAWL_*` variables that hold paths.
const PATH_VARS: [&str; 6] = [
    "SCRAWL_CAPTURES_DIR",
    "SCRAWL_CONFIG",
    "SCRAWL_HELPER",
    "SCRAWL_MOTION_TRACE",
    "SCRAWL_WHISPER_CLI",
    "SCRAWL_WHISPER_MODEL",
];

/// Whether the on-demand unit is up or on its way up (starting, or waiting to restart).
fn ondemand_unit_running() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "scrawl-ondemand.service"])
        .status()
        .is_ok_and(|s| s.success())
        || std::process::Command::new("systemctl")
            .args(["--user", "show", "-p", "ActiveState", "--value", "scrawl-ondemand.service"])
            .output()
            .is_ok_and(|o| matches!(String::from_utf8_lossy(&o.stdout).trim(), "activating" | "reloading"))
}

fn spawn_detached() -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::pretty_key;

    #[test]
    fn shortcuts_read_like_keys() {
        assert_eq!(pretty_key("ALT + SHIFT + R"), "Alt+Shift+R");
        assert_eq!(pretty_key("SUPER+CTRL+x"), "Super+Ctrl+x");
    }
}
