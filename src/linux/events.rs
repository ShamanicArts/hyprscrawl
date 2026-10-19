//! The user's input during a recording, without root, the `input` group or a plugin:
//!
//! - Lua injected into Hyprland (`hyprctl eval`) binds the mouse buttons and wheel as
//!   non-consuming, mods-ignoring binds and hooks `input.keyboard.key`; it appends one line per
//!   event to `$XDG_RUNTIME_DIR/scrawl/events.log`. Plain typing never leaves the compositor:
//!   only modifier keys and keys pressed with Ctrl/Alt/Super held are written.
//! - A 125 Hz sampler of socket1 `j/cursorpos` gives the cursor path (and Shift-ink).
//! - socket2 gives focus changes and `configreloaded`, after which the Lua is injected again
//!   (a reload wipes the Lua state).
//!
//! Hyprland 0.56.2 segfaults if one of two binds on the same key is `:remove()`d and the other
//! handle is touched later, so the binds are made once per Lua state and toggled with
//! `set_enabled` (see FINDINGS.md).

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;

use crate::capture::{LiveOverlayEvent, Mods, SharedCaptureTelemetry, mono_us, scrawl_log};

pub fn log_path() -> PathBuf {
    crate::linux::gsr::runtime_dir().join("events.log")
}

/// xkb keycodes (evdev + 8) of the modifier keys, left then right: Shift, Ctrl, Alt, Super. The
/// injected Lua lists the same keys in the same order.
const MOD_KEYS: [u32; 8] = [50, 62, 37, 105, 64, 108, 133, 134];
/// xkb keycode of the key in Z's place.
const KEY_Z: u32 = 52;

fn inject_lua(path: &str) -> String {
    let p = crate::linux::hypr::lua_str(path);
    format!(
        r#"
local function opened(p) local f = io.open(p, "a"); if f then f:setvbuf("line") end; return f end
local function kd(k) local ok, r = pcall(hl.is_key_down, k); return (ok and r) and 1 or 0 end
local modnames = {{ {{50, "Shift_L"}}, {{62, "Shift_R"}}, {{37, "Control_L"}}, {{105, "Control_R"}}, {{64, "Alt_L"}}, {{108, "Alt_R"}}, {{133, "Super_L"}}, {{134, "Super_R"}} }}
if scrawl_obs == nil then
  scrawl_obs = {{ on = false, binds = {{}}, mods = {{}} }}
  local o = scrawl_obs
  local function pos() local c = hl.get_cursor_pos(); if c then return string.format("%.1f %.1f", c.x, c.y) end; return "nan nan" end
  local function put(s) if o.on and o.f then o.f:write(s, "\n") end end
  local modkeys = {{ [50]=1, [62]=1, [37]=1, [105]=1, [64]=1, [108]=1, [133]=1, [134]=1 }}
  o.sub = hl.on("input.keyboard.key", function(code, time, state)
    if not o.on then return end
    if modkeys[code] then o.mods[code] = (state ~= 0) or nil; put(string.format("K %d %d %d", code, state, time)); return end
    if state ~= 1 then return end
    local chord = o.mods[37] or o.mods[105] or o.mods[64] or o.mods[108] or o.mods[133] or o.mods[134]
    if chord or code == 9 then put(string.format("K %d %d %d", code, state, time)) end
  end)
  local opts = {{ non_consuming = true, ignore_mods = true, transparent = true, submap_universal = true }}
  local function with(extra) local r = {{}} for k, v in pairs(opts) do r[k] = v end for k, v in pairs(extra) do r[k] = v end return r end
  for _, b in ipairs({{ "mouse:272", "mouse:273", "mouse:274" }}) do
    table.insert(o.binds, hl.bind(b, function() put("B " .. b .. " d " .. pos()) end, opts))
    table.insert(o.binds, hl.bind(b, function() put("B " .. b .. " u " .. pos()) end, with({{ release = true }})))
  end
  for _, b in ipairs({{ "mouse_up", "mouse_down", "mouse_left", "mouse_right" }}) do
    table.insert(o.binds, hl.bind(b, function() put("W " .. b .. " " .. pos()) end, opts))
  end
end
local o = scrawl_obs
if o.esc == nil then
  -- Upstream's Esc: cancels the recording, and (unlike the observers) is consumed while recording.
  o.esc = hl.bind("Escape", function() if o.on and o.f then o.f:write("X esc\n") end end, {{ submap_universal = true, description = "Scrawl: cancel the recording" }})
  table.insert(o.binds, o.esc)
end
if o.f then pcall(function() o.f:close() end) end
o.f = opened({p})
-- Modifiers already held (across a reload, or when the recording starts): the chord filter
-- needs them, and Rust gets them key by key so releasing one side leaves the other held.
o.mods = {{}}
local held = {{}}
for _, m in ipairs(modnames) do
  local d = kd(m[2]); if d == 1 then o.mods[m[1]] = true end
  table.insert(held, d)
end
for _, b in ipairs(o.binds) do b:set_enabled(true) end
o.on = true
if o.f then o.f:write("M ", table.concat(held, " "), "\n") end
"#
    )
}

const STOP_LUA: &str = r#"
if scrawl_obs then
  scrawl_obs.on = false
  for _, b in ipairs(scrawl_obs.binds) do b:set_enabled(false) end
  if scrawl_obs.f then pcall(function() scrawl_obs.f:close() end); scrawl_obs.f = nil end
end
"#;

/// Who may have the hook on. Injecting (including after a config reload) and switching off both
/// happen under this lock, so a reload can't switch the consuming Esc bind back on after the
/// recording let go, and nothing can inject after `close` (the daemon is exiting).
struct Hook {
    on: bool,
    closed: bool,
}

static HOOK: std::sync::Mutex<Hook> = std::sync::Mutex::new(Hook { on: false, closed: false });

fn hook() -> std::sync::MutexGuard<'static, Hook> {
    HOOK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn inject() -> Result<()> {
    let mut hook = hook();
    anyhow::ensure!(!hook.closed, "the daemon is shutting down");
    crate::linux::hypr::eval(&inject_lua(&log_path().display().to_string()))?;
    hook.on = true;
    Ok(())
}

/// Injects again after `hyprctl reload` wiped the Lua state, only while a recording owns the hook.
fn reinject() -> Result<()> {
    let hook = hook();
    if hook.on && !hook.closed {
        crate::linux::hypr::eval(&inject_lua(&log_path().display().to_string()))?;
    }
    Ok(())
}

/// Switches the hook off for good: the daemon is exiting, and no later start or reload may
/// inject it again.
pub fn close() {
    let mut hook = hook();
    hook.closed = true;
    hook.on = false;
    if let Err(e) = crate::linux::hypr::eval(STOP_LUA) {
        scrawl_log(format!("input hook stop: {e:#}"));
    }
}

/// Switches the hook and its binds off. Safe to call when nothing was injected (a new daemon calls
/// it to clear whatever an earlier one left behind).
pub fn disable() {
    let mut hook = hook();
    hook.on = false;
    if let Err(e) = crate::linux::hypr::eval(STOP_LUA) {
        scrawl_log(format!("input hook stop: {e:#}"));
    }
}

/// The running input source for one recording.
pub struct Source {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Source {
    pub fn start(telemetry: Arc<SharedCaptureTelemetry>) -> Result<Self> {
        let path = log_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, b"")?;
        let mut source = Self {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        // Anything that fails part way undoes what was started: no thread or Esc bind is left on.
        let started = (|| -> Result<()> {
            // Tail first, so the initial modifier line isn't missed.
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::End(0))?;
            let (stop, t) = (Arc::clone(&source.stop), Arc::clone(&telemetry));
            source.threads.push(std::thread::Builder::new().name("scrawl-input-tail".into()).spawn(move || {
                tail(file, &t, &stop);
            })?);
            inject()?;
            let (stop, t) = (Arc::clone(&source.stop), Arc::clone(&telemetry));
            source.threads.push(std::thread::Builder::new().name("scrawl-cursor".into()).spawn(move || {
                sample_cursor(&t, &stop);
            })?);
            let (stop, t) = (Arc::clone(&source.stop), Arc::clone(&telemetry));
            source.threads.push(std::thread::Builder::new().name("scrawl-hypr-events".into()).spawn(move || {
                hypr_events(&t, &stop);
            })?);
            Ok(())
        })();
        match started {
            Ok(()) => Ok(source),
            Err(e) => {
                source.stop();
                Err(e)
            }
        }
    }

    pub fn stop(self) {
        disable();
        // Let the last lines (the release that ended a drag) arrive.
        std::thread::sleep(Duration::from_millis(30));
        self.stop.store(true, Ordering::Release);
        for t in self.threads {
            // The socket2 reader blocks on the socket; don't wait for it.
            if t.thread().name() == Some("scrawl-hypr-events") {
                continue;
            }
            let _ = t.join();
        }
    }
}

fn tail(file: std::fs::File, telemetry: &SharedCaptureTelemetry, stop: &AtomicBool) {
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut held = Held::default();
    while !stop.load(Ordering::Acquire) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Ok(_) if !line.ends_with('\n') => {
                // Partial line: wait for the rest.
                let mut rest = String::new();
                while !rest.ends_with('\n') && !stop.load(Ordering::Acquire) {
                    if reader.read_line(&mut rest).unwrap_or(0) == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                line.push_str(&rest);
            }
            Ok(_) => {}
            Err(_) => break,
        }
        let arrival = telemetry.elapsed_ms();
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["M", keys @ ..] if keys.len() == MOD_KEYS.len() => {
                held = Held(std::array::from_fn(|i| keys[i] == "1"));
                telemetry.set_mods(held.mods());
            }
            ["K", code, state, time] => {
                let (Ok(code), Ok(state)) = (code.parse::<u32>(), state.parse::<u32>()) else {
                    continue;
                };
                let t = key_time(telemetry, time.parse().ok(), arrival);
                let down = state != 0;
                let mods = held.mods();
                if held.set(code, down) {
                    telemetry.set_mods(held.mods());
                } else if down && code == KEY_Z && mods.ctrl && !mods.alt && !mods.shift && !mods.logo {
                    // Upstream's ⌘Z: undo the last stroke (the app gets the key too).
                    let _ = telemetry.ui_sender.try_send(LiveOverlayEvent::UndoStroke);
                    continue;
                } else if down {
                    telemetry.key_down(t, code.saturating_sub(8));
                }
            }
            ["X", "esc"] => {
                let _ = telemetry.ui_sender.try_send(LiveOverlayEvent::CancelCapture);
            }
            ["B", btn, edge, x, y] => {
                let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else {
                    continue;
                };
                let right = *btn == "mouse:273";
                if *edge == "d" {
                    telemetry.pointer_down(arrival, x, y);
                } else {
                    telemetry.pointer_up(arrival, x, y, right);
                }
            }
            ["W", dir, x, y] => {
                let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else {
                    continue;
                };
                // Core Graphics' convention: positive is up / left.
                let (dx, dy) = match *dir {
                    "mouse_up" => (0.0, 1.0),
                    "mouse_down" => (0.0, -1.0),
                    "mouse_left" => (1.0, 0.0),
                    _ => (-1.0, 0.0),
                };
                telemetry.scroll(arrival, x, y, dx, dy);
            }
            _ => {}
        }
    }
}

/// Which modifier keys are down, each side on its own (in `MOD_KEYS` order): letting go of one
/// Shift while the other is still held keeps Shift held.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct Held([bool; 8]);

impl Held {
    /// Records a modifier key's state; false if `code` isn't a modifier key.
    fn set(&mut self, code: u32, down: bool) -> bool {
        let Some(i) = MOD_KEYS.iter().position(|&k| k == code) else {
            return false;
        };
        self.0[i] = down;
        true
    }

    fn mods(self) -> Mods {
        let k = self.0;
        Mods {
            shift: k[0] || k[1],
            ctrl: k[2] || k[3],
            alt: k[4] || k[5],
            logo: k[6] || k[7],
        }
    }
}

/// The key event's own time (the device's ms on `CLOCK_MONOTONIC`, 32-bit) when it agrees with
/// the arrival time; otherwise the arrival time (virtual keyboards send their own clocks).
fn key_time(telemetry: &SharedCaptureTelemetry, event_ms: Option<u64>, arrival: u64) -> u64 {
    let Some(ev) = event_ms else { return arrival };
    let now_ms = mono_us() / 1_000;
    let age = (now_ms as u32).wrapping_sub(ev as u32) as u64;
    if age > 500 {
        return arrival;
    }
    telemetry.t_of_mono((now_ms - age) * 1_000)
}

fn sample_cursor(telemetry: &SharedCaptureTelemetry, stop: &AtomicBool) {
    let period = Duration::from_millis(8);
    while !stop.load(Ordering::Acquire) {
        let started = std::time::Instant::now();
        if let Some(p) = crate::linux::hypr::cursor() {
            telemetry.cursor_sample(telemetry.elapsed_ms(), p.x, p.y);
        }
        let spent = started.elapsed();
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
}

fn hypr_events(telemetry: &SharedCaptureTelemetry, stop: &AtomicBool) {
    let result = crate::linux::hypr::events(|name, data| {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match name {
            "activewindow" => {
                if let Some((class, title)) = data.split_once(',')
                    && !class.is_empty()
                {
                    telemetry.app_change(
                        telemetry.elapsed_ms(),
                        class.to_owned(),
                        (!title.is_empty()).then(|| title.to_owned()),
                    );
                    telemetry.window_log_request.store(true, Ordering::Release);
                }
            }
            "configreloaded" => {
                scrawl_log("Hyprland config reloaded: injecting the input hook again");
                if let Err(e) = reinject() {
                    scrawl_log(format!("re-inject failed: {e:#}"));
                }
            }
            _ => {}
        }
        true
    });
    if let Err(e) = result {
        scrawl_log(format!("Hyprland event stream: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_sides_are_tracked_separately() {
        let mut held = Held::default();
        assert!(held.set(50, true)); // left Shift
        assert!(held.set(62, true)); // right Shift
        assert!(held.set(62, false));
        assert!(held.mods().shift, "left Shift is still down");
        assert!(held.set(50, false));
        assert!(!held.mods().shift);
        assert!(held.set(105, true)); // right Ctrl
        assert!(held.mods().ctrl && !held.mods().alt && !held.mods().logo);
        assert!(!held.set(KEY_Z, true), "Z isn't a modifier");
    }

    #[test]
    fn injected_lua_reports_every_modifier_key() {
        let lua = inject_lua("/x");
        for code in MOD_KEYS {
            assert!(lua.contains(&format!("{{{code}, \"")), "no seed for key {code}");
        }
    }
}
