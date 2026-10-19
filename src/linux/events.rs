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

use crate::capture::{Mods, SharedCaptureTelemetry, mono_us, scrawl_log};

pub fn log_path() -> PathBuf {
    crate::linux::gsr::runtime_dir().join("events.log")
}

/// xkb keycodes (evdev + 8) of the modifiers.
const SHIFT: [u32; 2] = [50, 62];
const CTRL: [u32; 2] = [37, 105];
const ALT: [u32; 2] = [64, 108];
const LOGO: [u32; 2] = [133, 134];

fn inject_lua(path: &str) -> String {
    let p = crate::linux::hypr::lua_str(path);
    format!(
        r#"
local function opened(p) local f = io.open(p, "a"); if f then f:setvbuf("line") end; return f end
local function kd(k) local ok, r = pcall(hl.is_key_down, k); return (ok and r) and 1 or 0 end
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
if o.f then pcall(function() o.f:close() end) end
o.f = opened({p})
o.mods = {{}}
for _, b in ipairs(o.binds) do b:set_enabled(true) end
o.on = true
if o.f then o.f:write(string.format("M %d %d %d %d\n", math.max(kd("Shift_L"), kd("Shift_R")), math.max(kd("Control_L"), kd("Control_R")), math.max(kd("Alt_L"), kd("Alt_R")), math.max(kd("Super_L"), kd("Super_R")))) end
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

pub fn inject() -> Result<()> {
    crate::linux::hypr::eval(&inject_lua(&log_path().display().to_string()))?;
    Ok(())
}

pub fn disable() {
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
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        // Tail first, so the initial modifier line isn't missed.
        let mut file = std::fs::File::open(&path)?;
        file.seek(SeekFrom::End(0))?;
        {
            let (stop, t) = (Arc::clone(&stop), Arc::clone(&telemetry));
            threads.push(std::thread::Builder::new().name("scrawl-input-tail".into()).spawn(move || {
                tail(file, &t, &stop);
            })?);
        }
        inject()?;
        {
            let (stop, t) = (Arc::clone(&stop), Arc::clone(&telemetry));
            threads.push(std::thread::Builder::new().name("scrawl-cursor".into()).spawn(move || {
                sample_cursor(&t, &stop);
            })?);
        }
        {
            let (stop, t) = (Arc::clone(&stop), Arc::clone(&telemetry));
            threads.push(std::thread::Builder::new().name("scrawl-hypr-events".into()).spawn(move || {
                hypr_events(&t, &stop);
            })?);
        }
        Ok(Self { stop, threads })
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

struct Pending {
    path: Vec<[f32; 2]>,
}

fn tail(file: std::fs::File, telemetry: &SharedCaptureTelemetry, stop: &AtomicBool) {
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut mods = Mods::default();
    let mut drag = Pending { path: Vec::new() };
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
            ["M", s, c, a, l] => {
                mods = Mods {
                    shift: *s == "1",
                    ctrl: *c == "1",
                    alt: *a == "1",
                    logo: *l == "1",
                };
                telemetry.set_mods(mods);
            }
            ["K", code, state, time] => {
                let (Ok(code), Ok(state)) = (code.parse::<u32>(), state.parse::<u32>()) else {
                    continue;
                };
                let t = key_time(telemetry, time.parse().ok(), arrival);
                let down = state != 0;
                if SHIFT.contains(&code) {
                    mods.shift = down;
                } else if CTRL.contains(&code) {
                    mods.ctrl = down;
                } else if ALT.contains(&code) {
                    mods.alt = down;
                } else if LOGO.contains(&code) {
                    mods.logo = down;
                } else if down {
                    telemetry.key_down(t, code.saturating_sub(8));
                    continue;
                }
                telemetry.set_mods(mods);
            }
            ["B", btn, edge, x, y] => {
                let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else {
                    continue;
                };
                let right = *btn == "mouse:273";
                if *edge == "d" {
                    drag.path.clear();
                    telemetry.pointer_down(arrival, x, y);
                } else {
                    let path = thin(std::mem::take(&mut drag.path), 64);
                    telemetry.pointer_up(arrival, x, y, right, path);
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
        // Collect the path of a drag in progress from the cursor the sampler last saw.
        if telemetry.drag_started_at().is_some() {
            let [px, py] = *telemetry.pointer_screen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (nx, ny, _, _) = telemetry.normalize_point(f64::from(px), f64::from(py));
            drag.path.push([nx, ny]);
        }
    }
}

fn thin(path: Vec<[f32; 2]>, max: usize) -> Vec<[f32; 2]> {
    if path.len() <= max {
        return path;
    }
    let step = path.len() as f64 / max as f64;
    (0..max).map(|i| path[(i as f64 * step) as usize]).collect()
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
                if let Err(e) = inject() {
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
