//! Linux (Hyprland) stand-in for `capture.rs`: same public surface the rest of Scrawl uses,
//! with gpu-screen-recorder for video+mic and Hyprland Lua binds + IPC for input
//! (`crate::linux::events`). The click/drag/ink rules are ported from upstream's event tap and
//! HID poll loop so bundles match the Mac ones.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::model::{
    CaptureMode, InteractionEvent, RgbaFrame, SCALE, Stroke, TargetFrame, TargetInfo,
};

pub const FRAME_SAMPLE_INTERVAL: Duration = Duration::from_millis(220);
/// Candidate frames only feed scene diffs and keyframe selection; the keyframes themselves are
/// decoded from `recording.mp4` at full size afterwards.
pub const CANDIDATE_EDGE: f64 = 480.0;
/// The candidates' memory ceiling, however long the capture runs.
pub const CANDIDATE_BUDGET_BYTES: usize = 384 << 20;

// ---- verbatim from upstream capture.rs -------------------------------------------------------

/// The candidate frames of a capture, bounded in memory. When full it drops every other frame
/// (keeping the first and the ones taken for a click, stroke or shortcut) and samples half as
/// often from then on, so a long capture keeps its whole timeline, evenly, at a coarser step.
pub struct CandidateBuffer {
    frames: Vec<(RgbaFrame, bool)>,
    cap: usize,
    budget: usize,
    interval: Duration,
}

impl CandidateBuffer {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            frames: Vec::new(),
            cap: 0,
            budget: budget_bytes,
            interval: FRAME_SAMPLE_INTERVAL,
        }
    }

    /// How often to sample now.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn push(&mut self, frame: RgbaFrame, urgent: bool) {
        if self.cap == 0 {
            self.cap = (self.budget / frame.rgba.len().max(1)).clamp(60, 1_200);
        }
        self.frames.push((frame, urgent));
        if self.frames.len() >= self.cap {
            self.thin();
        }
    }

    fn thin(&mut self) {
        let mut regular = 0_usize;
        let mut kept: Vec<(RgbaFrame, bool)> = std::mem::take(&mut self.frames)
            .into_iter()
            .enumerate()
            .filter(|(i, (_, urgent))| {
                let keep = *i == 0 || *urgent || regular.is_multiple_of(2);
                if !urgent {
                    regular += 1;
                }
                keep
            })
            .map(|(_, f)| f)
            .collect();
        // Mostly urgent frames (a storm of clicks): thin those too.
        if kept.len() > self.cap * 3 / 4 {
            kept = kept
                .into_iter()
                .enumerate()
                .filter(|(i, _)| i.is_multiple_of(2))
                .map(|(_, f)| f)
                .collect();
        }
        self.frames = kept;
        self.interval *= 2;
    }

    pub fn into_frames(self) -> Vec<RgbaFrame> {
        self.frames.into_iter().map(|(f, _)| f).collect()
    }
}

/// What a finished screen capture leaves behind.
pub struct ScreenRecording {
    /// Small candidate frames for keyframe selection (see `CANDIDATE_EDGE`).
    pub frames: Vec<RgbaFrame>,
    pub video: Option<PathBuf>,
    pub duration_ms: u64,
    /// The capture time (on the frames' clock) of the video's start.
    pub video_offset_ms: u64,
}

pub enum LiveOverlayEvent {
    /// ⌥⇧S (or the configured shortcut) went down; `hold::Switcher` decides what it means.
    CaptureShortcut,
    PasteLatestShortcut,
    UndoStroke,
    CancelCapture,
    RequestSnapshot,
    /// The iPhone mirror ended on its own (unplugged, never found, or access denied).
    PhoneEnded,
    /// The welcome window: a button, fresh permission states, or the helper exiting.
    Onboarding(crate::onboarding::Event),
    /// The Settings window: an action, fresh permission states, or the helper exiting.
    Settings(crate::settings::Event),
    /// A ScrawlKit recording was ingested from an iPhone and compiled into a `.scrawl` bundle.
    IngestedBundle {
        dir: PathBuf,
        bundle: Box<crate::model::ScrawlBundle>,
        status: Option<String>,
    },
}

pub fn capture_size(target: &TargetInfo, long_edge: f64) -> (u32, u32) {
    let max_dim = target.frame.width.max(target.frame.height).max(1.0);
    let scale = (long_edge / max_dim).min(2.0);
    (
        ((target.frame.width * scale).round() as u32).max(48) & !1,
        ((target.frame.height * scale).round() as u32).max(48) & !1,
    )
}

/// Writes all bundle artifacts (`scrawl.json`, `summary.md`, `contact-sheet.png`, `frames/frame-XX.png`)
/// into a new or specified `.scrawl` bundle directory.
pub fn write_bundle_artifacts(
    out_dir: &Path,
    bundle: &crate::model::ScrawlBundle,
    annotated_frames: &[(crate::model::Keyframe, RgbaFrame)],
    contact_sheet: &RgbaFrame,
    summary_md: &str,
) -> Result<()> {
    std::fs::create_dir_all(out_dir.join("frames"))
        .with_context(|| format!("could not create {}", out_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out_dir, std::fs::Permissions::from_mode(0o700));
    }

    thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = annotated_frames
            .iter()
            .map(|(kf, frame)| {
                scope.spawn(move || -> Result<()> {
                    crate::render::save_png(frame, &out_dir.join(&kf.file))
                })
            })
            .collect();
        crate::render::save_png(contact_sheet, &out_dir.join(&bundle.contact_sheet))?;
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("PNG encoding worker panicked"))??;
        }
        Ok(())
    })?;

    let manifest_bytes = serde_json::to_vec_pretty(bundle)?;
    std::fs::write(out_dir.join("scrawl.json"), manifest_bytes)?;
    std::fs::write(out_dir.join("summary.md"), summary_md.as_bytes())?;

    Ok(())
}

// ---- Linux ------------------------------------------------------------------------------------

pub fn log_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("scrawl")
}

pub fn scrawl_log(msg: impl AsRef<str>) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let line = format!("[{ts}] {}\n", msg.as_ref());
    eprint!("{line}");
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("scrawl.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// No TCC on Linux: capture needs no grant.
pub fn screen_capture_granted() -> bool {
    true
}
pub fn request_screen_capture() {}
pub fn request_listen_access() {}
pub fn accessibility_granted() -> bool {
    true
}

/// Global pointer position in layout px.
pub fn current_mouse_screen() -> Option<[f32; 2]> {
    crate::linux::hypr::cursor().map(|p| [p.x as f32, p.y as f32])
}

pub fn frontmost_external_pid() -> Option<i32> {
    crate::linux::hypr::active_window()
        .map(|w| w.pid)
        .filter(|&pid| pid != std::process::id() as i32)
}

/// `(pid, app, bundle id)`: Hyprland's class is the closest thing to a bundle id.
pub fn frontmost_app_info() -> Option<(i32, String, Option<String>)> {
    crate::linux::hypr::active_window().map(|w| (w.pid, w.class.clone(), Some(w.class)))
}

pub fn is_frontmost_terminal() -> bool {
    let Some((_pid, app, _)) = frontmost_app_info() else {
        return false;
    };
    let a = app.to_ascii_lowercase();
    [
        "ghostty", "terminal", "kitty", "wezterm", "alacritty", "foot", "warp", "konsole",
    ]
    .iter()
    .any(|needle| a.contains(needle))
}

fn monitor_target(m: &crate::linux::hypr::Monitor) -> TargetInfo {
    let (w, h) = m.logical_size();
    TargetInfo {
        mode: CaptureMode::Display,
        app: None,
        bundle_id: None,
        window_title: None,
        window_id: None,
        display_id: m.id.max(0) as u32,
        frame: TargetFrame {
            x: m.x,
            y: m.y,
            width: w,
            height: h,
            scale: if m.scale > 0.0 { m.scale } else { 1.0 },
        },
    }
}

/// The monitor under `point` (default: the pointer), or a window whose class/title contains
/// `window_query`.
pub fn discover_target_at(window_query: Option<&str>, point: Option<[f64; 2]>) -> TargetInfo {
    let monitors = crate::linux::hypr::monitors().unwrap_or_default();
    if let Some(q) = window_query.map(str::to_ascii_lowercase)
        && let Ok(clients) = crate::linux::hypr::visible_clients()
        && let Some(c) = clients.iter().find(|c| {
            c.class.to_ascii_lowercase().contains(&q) || c.title.to_ascii_lowercase().contains(&q)
        })
    {
        let scale = monitors
            .iter()
            .find(|m| m.id == c.monitor)
            .map_or(1.0, |m| m.scale);
        return TargetInfo {
            mode: CaptureMode::Window,
            app: Some(c.class.clone()),
            bundle_id: Some(c.class.clone()),
            window_title: Some(c.title.clone()),
            window_id: Some(c.id()),
            display_id: c.monitor.max(0) as u32,
            frame: TargetFrame {
                x: c.at[0],
                y: c.at[1],
                width: c.size[0],
                height: c.size[1],
                scale,
            },
        };
    }
    let p = point.or_else(|| current_mouse_screen().map(|[x, y]| [f64::from(x), f64::from(y)]));
    let m = p
        .and_then(|[x, y]| monitors.iter().find(|m| m.contains(x, y)))
        .or_else(|| monitors.iter().find(|m| m.focused))
        .or(monitors.first());
    match m {
        Some(m) => monitor_target(m),
        None => TargetInfo {
            mode: CaptureMode::Display,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: 0,
            frame: TargetFrame {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
                scale: 1.0,
            },
        },
    }
}

pub fn discover_target(window_query: Option<&str>) -> TargetInfo {
    discover_target_at(window_query, None)
}

/// A region in global layout px (from `slurp` or the picker).
pub fn region_target(x: f64, y: f64, w: f64, h: f64) -> TargetInfo {
    let mut t = discover_target_at(None, Some([x + w / 2.0, y + h / 2.0]));
    t.mode = CaptureMode::Region;
    t.frame.x = x;
    t.frame.y = y;
    t.frame.width = w;
    t.frame.height = h;
    t
}

/// Microseconds on `CLOCK_MONOTONIC` (the clock `Instant` uses on Linux, and gsr's `.ts`).
pub fn mono_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

/// `CLOCK_MONOTONIC` µs of an `Instant` in the past.
pub fn instant_mono_us(at: Instant) -> u64 {
    mono_us().saturating_sub(at.elapsed().as_micros() as u64)
}

/// A screen + microphone recording through gpu-screen-recorder.
pub struct ScreenCaptureSession {
    started_at: Instant,
    recorder: Option<crate::linux::gsr::Recorder>,
    video: Option<PathBuf>,
    _recording: crate::retention::Recording,
}

impl ScreenCaptureSession {
    pub fn start(
        target: TargetInfo,
        video_output_path: Option<PathBuf>,
        _excluded_window_ids: Arc<Mutex<Vec<u32>>>,
    ) -> Self {
        Self::start_at(target, video_output_path, Instant::now())
    }

    /// Starts recording with `started_at` as the bundle's time zero (shared with the input log).
    pub fn start_at(target: TargetInfo, video_output_path: Option<PathBuf>, started_at: Instant) -> Self {
        let recorder = video_output_path.as_ref().and_then(|path| {
            crate::linux::gsr::Recorder::start(&target, path)
                .inspect_err(|e| scrawl_log(format!("gpu-screen-recorder failed to start: {e:#}")))
                .ok()
        });
        Self {
            started_at,
            recorder,
            video: video_output_path,
            _recording: crate::retention::Recording::start(),
        }
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn is_recording(&self) -> bool {
        self.recorder.as_ref().is_some_and(|r| r.is_running())
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub fn stop(mut self) -> ScreenRecording {
        let duration_ms = self.elapsed_ms();
        let t0_us = instant_mono_us(self.started_at);
        let (video, first_frame_us) = match self.recorder.take() {
            Some(rec) => match rec.stop() {
                Ok(done) => (Some(done.path), done.first_frame_mono_us),
                Err(e) => {
                    scrawl_log(format!("gpu-screen-recorder stop: {e:#}"));
                    (self.video.clone().filter(|p| p.is_file()), None)
                }
            },
            None => (None, None),
        };
        let video_offset_ms = first_frame_us.map_or(0, |ff| ff.saturating_sub(t0_us) / 1_000);
        let frames = video
            .as_deref()
            .map(|v| candidates(v, video_offset_ms, duration_ms))
            .unwrap_or_default();
        ScreenRecording {
            frames,
            video,
            duration_ms,
            video_offset_ms,
        }
    }
}

/// Small frames every 220 ms (coarser for long captures, so they stay within the budget),
/// stamped on the bundle clock.
fn candidates(video: &Path, offset_ms: u64, duration_ms: u64) -> Vec<RgbaFrame> {
    let interval = (duration_ms / 700).max(FRAME_SAMPLE_INTERVAL.as_millis() as u64);
    let started = Instant::now();
    match crate::helper::frames(video, crate::helper::FrameTimes::Every(interval), CANDIDATE_EDGE as u32) {
        Ok(raw) => {
            let mut buf = CandidateBuffer::new(CANDIDATE_BUDGET_BYTES);
            for mut f in raw {
                f.t_ms += offset_ms;
                buf.push(f, false);
            }
            let frames = buf.into_frames();
            scrawl_log(format!(
                "candidates: {} frames every {interval} ms in {} ms",
                frames.len(),
                started.elapsed().as_millis()
            ));
            frames
        }
        Err(e) => {
            scrawl_log(format!("candidate decode failed: {e:#}"));
            Vec::new()
        }
    }
}

/// Modifier state as seen by the Lua key hook.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

impl Mods {
    pub fn pure_shift(self) -> bool {
        self.shift && !self.ctrl && !self.alt && !self.logo
    }
}

/// Shared state collected by the input source during an active capture session.
pub struct SharedCaptureTelemetry {
    pub active: AtomicBool,
    pub selecting: AtomicBool,
    pub shift_held: AtomicBool,
    pub started_at: Mutex<Instant>,
    pub picker_rect: Mutex<Option<([f32; 4], u8)>>,
    pub target_frame: Mutex<TargetFrame>,
    pub pointer_screen: Mutex<[f32; 2]>,
    pub mods: Mutex<Mods>,
    pub events: Mutex<Vec<InteractionEvent>>,
    pub strokes: Mutex<Vec<Stroke>>,
    pub live_ink: Mutex<Option<Stroke>>,
    /// Raised when a click, ink stroke or shortcut starts, so the window log snapshots now.
    pub window_log_request: Arc<AtomicBool>,
    pub ui_sender: flume::Sender<LiveOverlayEvent>,
    /// Shift was down when the session started (the record shortcut includes it): no ink until
    /// it is released once.
    await_shift_release: AtomicBool,
    prev_shift_ink: AtomicBool,
    drag_start: Mutex<Option<(u64, f32, f32, TargetFrame)>>,
    /// Increments on every stroke change, for the overlay.
    pub ink_generation: AtomicU64,
}

impl SharedCaptureTelemetry {
    pub fn new(ui_sender: flume::Sender<LiveOverlayEvent>) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(false),
            selecting: AtomicBool::new(false),
            shift_held: AtomicBool::new(false),
            started_at: Mutex::new(Instant::now()),
            picker_rect: Mutex::new(None),
            target_frame: Mutex::new(TargetFrame {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
                scale: 1.0,
            }),
            pointer_screen: Mutex::new([0.0, 0.0]),
            mods: Mutex::new(Mods::default()),
            events: Mutex::new(Vec::new()),
            strokes: Mutex::new(Vec::new()),
            live_ink: Mutex::new(None),
            window_log_request: Arc::new(AtomicBool::new(false)),
            ui_sender,
            await_shift_release: AtomicBool::new(false),
            prev_shift_ink: AtomicBool::new(false),
            drag_start: Mutex::new(None),
            ink_generation: AtomicU64::new(0),
        })
    }

    pub fn set_picker_rect(&self, rect: Option<([f32; 4], u8)>) {
        *lock(&self.picker_rect) = rect;
    }

    pub fn reset_for_session(&self, frame: TargetFrame, started_at: Instant, mods: Mods) {
        self.selecting.store(false, Ordering::Release);
        *lock(&self.picker_rect) = None;
        *lock(&self.started_at) = started_at;
        *lock(&self.target_frame) = frame;
        *lock(&self.mods) = mods;
        lock(&self.events).clear();
        lock(&self.strokes).clear();
        *lock(&self.live_ink) = None;
        *lock(&self.drag_start) = None;
        self.shift_held.store(false, Ordering::Release);
        self.await_shift_release.store(mods.shift, Ordering::Release);
        self.prev_shift_ink.store(false, Ordering::Release);
        self.window_log_request.store(false, Ordering::Release);
        self.active.store(true, Ordering::Release);
        self.ink_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn elapsed_ms(&self) -> u64 {
        lock(&self.started_at).elapsed().as_millis() as u64
    }

    /// Bundle time of a `CLOCK_MONOTONIC` µs stamp.
    pub fn t_of_mono(&self, mono: u64) -> u64 {
        mono.saturating_sub(instant_mono_us(*lock(&self.started_at))) / 1_000
    }

    pub fn contains_screen_point(&self, screen_x: f64, screen_y: f64) -> bool {
        let frame = *lock(&self.target_frame);
        frame.width >= 16.0
            && frame.height >= 16.0
            && frame.x > -10_000.0
            && screen_x >= frame.x
            && screen_x <= frame.x + frame.width
            && screen_y >= frame.y
            && screen_y <= frame.y + frame.height
    }

    pub fn normalize_point(&self, screen_x: f64, screen_y: f64) -> (f32, f32, u32, u32) {
        let frame = *lock(&self.target_frame);
        let nx = ((screen_x - frame.x) / frame.width.max(1.0)).clamp(0.0, 1.0) as f32;
        let ny = ((screen_y - frame.y) / frame.height.max(1.0)).clamp(0.0, 1.0) as f32;
        (
            nx,
            ny,
            frame.width.round().max(1.0) as u32,
            frame.height.round().max(1.0) as u32,
        )
    }

    fn pure_shift(&self) -> bool {
        lock(&self.mods).pure_shift()
    }

    /// A mouse button went down at a global point (upstream `pointer_down`).
    pub fn pointer_down(&self, t_ms: u64, screen_x: f64, screen_y: f64) {
        if !self.active.load(Ordering::Acquire) || self.pure_shift() {
            return;
        }
        if !self.contains_screen_point(screen_x, screen_y) {
            return;
        }
        let (nx, ny, _, _) = self.normalize_point(screen_x, screen_y);
        self.window_log_request.store(true, Ordering::Release);
        let start_frame = *lock(&self.target_frame);
        *lock(&self.drag_start) = Some((t_ms, nx, ny, start_frame));
    }

    /// A mouse button came up: records the click or drag that `pointer_down` began.
    pub fn pointer_up(&self, t_ms: u64, screen_x: f64, screen_y: f64, right: bool, path: Vec<[f32; 2]>) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some((t0, x0, y0, start_frame)) = lock(&self.drag_start).take() else {
            return;
        };
        let end_frame = *lock(&self.target_frame);
        if !same_target_frame(start_frame, end_frame) {
            return;
        }
        let (nx, ny, fw, fh) = self.normalize_point(screen_x, screen_y);
        let dist_px = ((nx - x0) * fw as f32).hypot((ny - y0) * fh as f32);
        let _ = self.ui_sender.try_send(LiveOverlayEvent::RequestSnapshot);
        let event = if dist_px < 12.0 {
            InteractionEvent::Click {
                t: t0,
                x: x0,
                y: y0,
                button: Some(if right { "right" } else { "left" }.into()),
                label: None,
                role: None,
            }
        } else {
            InteractionEvent::Drag {
                t: t0,
                t1: t_ms,
                x0,
                y0,
                x1: nx,
                y1: ny,
                label: None,
                button: None,
                path,
            }
        };
        lock(&self.events).push(event);
    }

    /// The drag in progress, if any: its start time.
    pub fn drag_started_at(&self) -> Option<u64> {
        lock(&self.drag_start).as_ref().map(|d| d.0)
    }

    pub fn scroll(&self, t_ms: u64, screen_x: f64, screen_y: f64, dx: f32, dy: f32) {
        if !self.active.load(Ordering::Acquire) || !self.contains_screen_point(screen_x, screen_y) {
            return;
        }
        let (nx, ny, _, _) = self.normalize_point(screen_x, screen_y);
        lock(&self.events).push(InteractionEvent::Scroll {
            t: t_ms,
            x: nx,
            y: ny,
            dx,
            dy,
            label: None,
        });
    }

    /// A key went down. Only modifier chords are logged (never ordinary typing or secrets).
    pub fn key_down(&self, t_ms: u64, evdev: u32) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let m = *lock(&self.mods);
        if !(m.ctrl || m.logo) {
            return;
        }
        let Some(key_name) = key_name(evdev) else {
            return;
        };
        let mut chord = String::new();
        if m.ctrl {
            chord.push('⌃');
        }
        if m.alt {
            chord.push('⌥');
        }
        if m.shift {
            chord.push('⇧');
        }
        if m.logo {
            chord.push('⌘');
        }
        chord.push_str(key_name);
        self.window_log_request.store(true, Ordering::Release);
        lock(&self.events).push(InteractionEvent::Key {
            t: t_ms,
            shortcut: chord,
        });
    }

    pub fn set_mods(&self, mods: Mods) {
        let was_shift = std::mem::replace(&mut *lock(&self.mods), mods).shift;
        if was_shift && !mods.shift {
            self.await_shift_release.store(false, Ordering::Release);
        }
    }

    pub fn app_change(&self, t_ms: u64, app: String, title: Option<String>) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        lock(&self.events).push(InteractionEvent::AppChange {
            t: t_ms,
            app,
            window_title: title,
        });
    }

    /// One cursor sample (upstream's HID poll loop body): Shift held and moving inside the
    /// target draws a persistent stroke.
    pub fn cursor_sample(&self, t_ms: u64, x: f64, y: f64) {
        *lock(&self.pointer_screen) = [x as f32, y as f32];
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let inside_target = self.contains_screen_point(x, y);
        let pure_shift = self.pure_shift() && !self.await_shift_release.load(Ordering::Acquire);
        let prev_shift_ink = self.prev_shift_ink.load(Ordering::Acquire);

        if pure_shift && inside_target {
            self.shift_held.store(true, Ordering::Release);
            let (nx, ny, fw, fh) = self.normalize_point(x, y);
            let sx = (nx * SCALE as f32).round() as i32;
            let sy = (ny * SCALE as f32).round() as i32;
            let mut guard = lock(&self.live_ink);
            if guard.is_none() {
                self.window_log_request.store(true, Ordering::Release);
            }
            let stroke = guard.get_or_insert_with(|| Stroke {
                t0: t_ms,
                t1: t_ms,
                fw,
                fh,
                on: None,
                pin: None,
                laser: false,
                pts: Vec::new(),
            });
            let dt = t_ms.saturating_sub(stroke.t0) as i32;
            let should_push = stroke.pts.chunks_exact(3).last().is_none_or(|prev| {
                let dx = (sx - prev[0]) as f32 / SCALE as f32 * fw as f32;
                let dy = (sy - prev[1]) as f32 / SCALE as f32 * fh as f32;
                dx.hypot(dy) >= 1.5
            });
            if should_push && stroke.pts.len() / 3 < 2_400 {
                stroke.pts.extend_from_slice(&[sx, sy, dt]);
                stroke.t1 = t_ms;
                self.ink_generation.fetch_add(1, Ordering::AcqRel);
            }
        } else if !pure_shift && prev_shift_ink {
            self.shift_held.store(false, Ordering::Release);
            if let Some(stroke) = lock(&self.live_ink).take()
                && stroke.pts.len() >= 6
            {
                let first = &stroke.pts[0..2];
                let max_span_px = stroke
                    .pts
                    .chunks_exact(3)
                    .map(|p| {
                        let dx = (p[0] - first[0]) as f32 / SCALE as f32 * stroke.fw as f32;
                        let dy = (p[1] - first[1]) as f32 / SCALE as f32 * stroke.fh as f32;
                        dx.hypot(dy)
                    })
                    .fold(0.0_f32, f32::max);
                // Ignore stationary Shift taps (e.g. typing a capital letter without moving the mouse).
                if max_span_px >= 6.0 {
                    lock(&self.strokes).push(stroke);
                    let _ = self.ui_sender.try_send(LiveOverlayEvent::RequestSnapshot);
                }
            }
            self.ink_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.prev_shift_ink.store(pure_shift, Ordering::Release);
    }

    /// Strokes finished so far plus the one being drawn, for the overlay.
    pub fn ink_snapshot(&self) -> (Vec<Stroke>, Option<Stroke>) {
        (lock(&self.strokes).clone(), lock(&self.live_ink).clone())
    }

    pub fn undo_stroke(&self) {
        lock(&self.strokes).pop();
        self.ink_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn finish_session(&self) -> (Vec<InteractionEvent>, Vec<Stroke>) {
        self.active.store(false, Ordering::Release);
        self.selecting.store(false, Ordering::Release);
        *lock(&self.drag_start) = None;
        if let Some(ink) = lock(&self.live_ink).take()
            && ink.pts.len() >= 3
        {
            lock(&self.strokes).push(ink);
        }
        let mut events = std::mem::take(&mut *lock(&self.events));
        events.sort_by_key(event_t);
        let strokes = std::mem::take(&mut *lock(&self.strokes));
        self.ink_generation.fetch_add(1, Ordering::AcqRel);
        (events, strokes)
    }
}

fn event_t(e: &InteractionEvent) -> u64 {
    match e {
        InteractionEvent::Click { t, .. }
        | InteractionEvent::Drag { t, .. }
        | InteractionEvent::Scroll { t, .. }
        | InteractionEvent::Point { t, .. }
        | InteractionEvent::Key { t, .. }
        | InteractionEvent::AppChange { t, .. }
        | InteractionEvent::AppEvent { t, .. } => *t,
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn same_target_frame(a: TargetFrame, b: TargetFrame) -> bool {
    (a.x - b.x).abs() <= 0.5
        && (a.y - b.y).abs() <= 0.5
        && (a.width - b.width).abs() <= 0.5
        && (a.height - b.height).abs() <= 0.5
}

/// Names for evdev keycodes in logged shortcuts.
pub fn key_name(evdev: u32) -> Option<&'static str> {
    Some(match evdev {
        16 => "Q", 17 => "W", 18 => "E", 19 => "R", 20 => "T", 21 => "Y", 22 => "U", 23 => "I",
        24 => "O", 25 => "P", 30 => "A", 31 => "S", 32 => "D", 33 => "F", 34 => "G", 35 => "H",
        36 => "J", 37 => "K", 38 => "L", 44 => "Z", 45 => "X", 46 => "C", 47 => "V", 48 => "B",
        49 => "N", 50 => "M", 2 => "1", 3 => "2", 4 => "3", 5 => "4", 6 => "5", 7 => "6",
        8 => "7", 9 => "8", 10 => "9", 11 => "0", 28 => "↩", 15 => "⇥", 57 => "Space",
        14 => "⌫", 1 => "⎋", 103 => "↑", 108 => "↓", 105 => "←", 106 => "→",
        59 => "F1", 60 => "F2", 61 => "F3", 62 => "F4", 63 => "F5", 64 => "F6",
        65 => "F7", 66 => "F8", 67 => "F9", 68 => "F10", 87 => "F11", 88 => "F12",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry() -> Arc<SharedCaptureTelemetry> {
        let (tx, _rx) = flume::unbounded();
        let t = SharedCaptureTelemetry::new(tx);
        t.reset_for_session(
            TargetFrame { x: 100.0, y: 0.0, width: 1000.0, height: 500.0, scale: 1.0 },
            Instant::now(),
            Mods::default(),
        );
        t
    }

    #[test]
    fn click_drag_and_shift_ink() {
        let t = telemetry();
        t.pointer_down(10, 600.0, 250.0);
        t.pointer_up(80, 603.0, 251.0, false, Vec::new());
        t.pointer_down(200, 200.0, 100.0);
        t.pointer_up(400, 700.0, 300.0, false, vec![[0.3, 0.4]]);
        t.set_mods(Mods { shift: true, ..Mods::default() });
        for (i, x) in [300.0, 320.0, 340.0, 360.0].iter().enumerate() {
            t.cursor_sample(500 + i as u64 * 16, *x, 200.0);
        }
        // Shift-clicks are ink, not clicks.
        t.pointer_down(560, 360.0, 200.0);
        t.set_mods(Mods::default());
        t.cursor_sample(600, 360.0, 200.0);
        let (events, strokes) = t.finish_session();
        assert!(matches!(events[0], InteractionEvent::Click { t: 10, .. }));
        assert!(matches!(events[1], InteractionEvent::Drag { t: 200, t1: 400, .. }));
        assert_eq!(events.len(), 2);
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].pts.len(), 12);
        assert_eq!(strokes[0].t0, 500);
    }

    #[test]
    fn shift_from_the_shortcut_does_not_draw() {
        let (tx, _rx) = flume::unbounded();
        let t = SharedCaptureTelemetry::new(tx);
        t.reset_for_session(
            TargetFrame { x: 0.0, y: 0.0, width: 1000.0, height: 500.0, scale: 1.0 },
            Instant::now(),
            Mods { shift: true, alt: true, ..Mods::default() },
        );
        t.set_mods(Mods { shift: true, ..Mods::default() });
        t.cursor_sample(10, 100.0, 100.0);
        t.cursor_sample(30, 200.0, 100.0);
        assert!(t.ink_snapshot().1.is_none());
        t.set_mods(Mods::default());
        t.set_mods(Mods { shift: true, ..Mods::default() });
        t.cursor_sample(50, 100.0, 100.0);
        t.cursor_sample(70, 200.0, 100.0);
        assert!(t.ink_snapshot().1.is_some());
    }

    #[test]
    fn only_chords_are_logged() {
        let t = telemetry();
        t.key_down(5, 30);
        t.set_mods(Mods { ctrl: true, ..Mods::default() });
        t.key_down(9, 44);
        let (events, _) = t.finish_session();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], InteractionEvent::Key { shortcut, .. } if shortcut == "⌃Z"));
    }

    #[test]
    fn candidate_buffer_thins() {
        let mut buf = CandidateBuffer::new(60 * 4 * 4);
        for i in 0..200 {
            buf.push(RgbaFrame::new(i, 2, 2, vec![0; 16]), false);
        }
        assert!(buf.len() < 200);
    }
}
