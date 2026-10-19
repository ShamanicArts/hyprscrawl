//! Hyprland IPC: instance resolution, socket1 requests (`j/cursorpos`, `j/clients`, `eval`, …)
//! and the socket2 event stream.
//!
//! Shells (agents, terminals started before a re-login) can carry a stale
//! `HYPRLAND_INSTANCE_SIGNATURE`, so the instance is verified with a request and otherwise the
//! newest live one under `$XDG_RUNTIME_DIR/hypr` is used. `SCRAWL_HYPR_SIG` overrides both.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

fn hypr_dir() -> PathBuf {
    runtime_dir().join("hypr")
}

static SIG: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn probe(sig: &str) -> bool {
    request_on(sig, "version").is_ok_and(|r| r.contains("Hyprland"))
}

/// The live instance signature (cached; `reset()` after a connection error).
pub fn signature() -> Result<String> {
    let cell = SIG.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(sig) = guard.as_ref() {
        return Ok(sig.clone());
    }
    let mut candidates: Vec<String> = Vec::new();
    for var in ["SCRAWL_HYPR_SIG", "HYPRLAND_INSTANCE_SIGNATURE"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            candidates.push(v);
        }
    }
    // Each instance dir's `hyprland.lock` holds its pid and Wayland socket. Old sessions leave
    // dirs behind (dead pids), and a nested Hyprland has its own socket: prefer the live
    // instance serving this session's `$WAYLAND_DISPLAY`, then any live one, newest first.
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let mut dirs: Vec<(bool, std::time::SystemTime, String)> = std::fs::read_dir(hypr_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join(".socket.sock").exists())
        .filter_map(|e| {
            let lock = std::fs::read_to_string(e.path().join("hyprland.lock")).unwrap_or_default();
            let mut lines = lock.lines();
            let pid: i32 = lines.next()?.trim().parse().ok()?;
            let socket = lines.next().unwrap_or("").trim().to_owned();
            if unsafe { libc::kill(pid, 0) } != 0 {
                return None;
            }
            let m = e.metadata().ok()?.modified().ok()?;
            Some((
                !wayland.is_empty() && socket == wayland,
                m,
                e.file_name().to_string_lossy().into_owned(),
            ))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let dirs: Vec<(std::time::SystemTime, String)> = dirs.into_iter().map(|(_, m, s)| (m, s)).collect();
    // An explicit override is final; otherwise prefer the env's instance, then the newest live.
    if let Ok(v) = std::env::var("SCRAWL_HYPR_SIG")
        && !v.is_empty()
    {
        *guard = Some(v.clone());
        return Ok(v);
    }
    candidates.extend(dirs.into_iter().map(|(_, s)| s));
    for sig in candidates {
        if probe(&sig) {
            *guard = Some(sig.clone());
            return Ok(sig);
        }
    }
    bail!("No running Hyprland instance found (is this a Hyprland session?)")
}

pub fn reset() {
    if let Some(cell) = SIG.get() {
        *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

fn socket(sig: &str, name: &str) -> PathBuf {
    hypr_dir().join(sig).join(name)
}

fn request_on(sig: &str, cmd: &str) -> Result<String> {
    let mut s = UnixStream::connect(socket(sig, ".socket.sock"))
        .with_context(|| format!("Could not connect to Hyprland {sig}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(cmd.as_bytes())?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out)
}

/// One socket1 request (`hyprctl`'s wire format, e.g. `j/clients`), retried once on a fresh
/// instance lookup.
pub fn request(cmd: &str) -> Result<String> {
    let sig = signature()?;
    match request_on(&sig, cmd) {
        Ok(r) => Ok(r),
        Err(_) => {
            reset();
            request_on(&signature()?, cmd)
        }
    }
}

pub fn json<T: for<'de> Deserialize<'de>>(cmd: &str) -> Result<T> {
    let raw = request(&format!("j/{cmd}"))?;
    serde_json::from_str(&raw).with_context(|| format!("Bad Hyprland reply to {cmd}: {raw:.200}"))
}

/// Runs Lua in the compositor (`hyprctl eval`). Errors come back as text starting `error`.
pub fn eval(lua: &str) -> Result<String> {
    let reply = request(&format!("eval {lua}"))?;
    if reply.trim_start().starts_with("error") {
        bail!("hyprctl eval: {}", reply.trim());
    }
    Ok(reply)
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct CursorPos {
    pub x: f64,
    pub y: f64,
}

/// A persistent socket1 connection is not supported (Hyprland closes after each reply), but a
/// connect + `j/cursorpos` costs ~30 µs, so a 120 Hz sampler is cheap.
pub fn cursor() -> Option<CursorPos> {
    json("cursorpos").ok()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    #[serde(default)]
    pub transform: i64,
    #[serde(default)]
    pub focused: bool,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WorkspaceRef,
    #[serde(rename = "specialWorkspace", default)]
    pub special_workspace: Option<WorkspaceRef>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

impl Monitor {
    /// Size in layout px (what cursor positions and window geometry use).
    pub fn logical_size(&self) -> (f64, f64) {
        let (w, h) = if self.transform % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        ((w / s).round(), (h / s).round())
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        let (w, h) = self.logical_size();
        x >= self.x && x < self.x + w && y >= self.y && y < self.y + h
    }
}

pub fn monitors() -> Result<Vec<Monitor>> {
    json("monitors")
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub address: String,
    #[serde(default)]
    pub mapped: bool,
    #[serde(default)]
    pub hidden: bool,
    pub at: [f64; 2],
    pub size: [f64; 2],
    pub workspace: WorkspaceRef,
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub fullscreen: serde_json::Value,
    #[serde(default)]
    pub monitor: i64,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pid: i32,
    #[serde(rename = "focusHistoryID", default)]
    pub focus_history_id: i64,
}

impl Client {
    /// The low 32 bits of the window's address: stable for its lifetime and unique enough.
    pub fn id(&self) -> u32 {
        u64::from_str_radix(self.address.trim_start_matches("0x"), 16).unwrap_or(0) as u32
    }
}

pub fn clients() -> Result<Vec<Client>> {
    json("clients")
}

/// Mapped windows on the visible workspaces, front to back (fullscreen, then pinned/floating,
/// then tiled; most recently focused first within each).
pub fn visible_clients() -> Result<Vec<Client>> {
    let mons = monitors()?;
    let visible: Vec<i64> = mons
        .iter()
        .flat_map(|m| {
            let mut v = vec![m.active_workspace.id];
            if let Some(s) = &m.special_workspace
                && s.id != 0
            {
                v.push(s.id);
            }
            v
        })
        .collect();
    let mut cs: Vec<Client> = clients()?
        .into_iter()
        .filter(|c| c.mapped && !c.hidden && (c.pinned || visible.contains(&c.workspace.id)))
        .collect();
    let fs = |c: &Client| c.fullscreen.as_i64().unwrap_or(0) > 0 || c.fullscreen.as_bool() == Some(true);
    let special = |c: &Client| c.workspace.id < 0;
    cs.sort_by_key(|c| {
        (
            !fs(c),
            !special(c),
            !(c.floating || c.pinned),
            c.focus_history_id,
        )
    });
    Ok(cs)
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ActiveWindow {
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pid: i32,
}

pub fn active_window() -> Option<ActiveWindow> {
    json::<ActiveWindow>("activewindow").ok().filter(|w| w.pid > 0)
}

/// The socket2 event stream: calls `on_event(name, data)` for each `name>>data` line until the
/// callback returns `false` or the connection drops (then returns).
pub fn events(mut on_event: impl FnMut(&str, &str) -> bool) -> Result<()> {
    let sig = signature()?;
    let stream = UnixStream::connect(socket(&sig, ".socket2.sock"))
        .context("Could not connect to Hyprland's event socket")?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if let Some((name, data)) = line.split_once(">>")
            && !on_event(name, data)
        {
            break;
        }
    }
    Ok(())
}

/// Quotes a string as a Lua literal.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
