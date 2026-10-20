//! The live ink overlay: one full-monitor `zwlr_layer_shell_v1` surface (layer overlay,
//! namespace `scrawl-overlay`), click-through (empty input region, no keyboard), drawn on the
//! CPU into `wl_shm` buffers with tiny-skia. It shows Shift-ink as it is drawn and a brief red
//! edge glow when recording starts (upstream's start cue).
//!
//! The stroke geometry and size are upstream's (`sketch::freehand_outline`, the size rule in
//! `render::annotate_keyframe_in_place`), computed at the monitor's physical size, so the ink
//! later rendered onto keyframes lands exactly over the ink that is in the video (gsr's kms
//! capture includes every layer; Hyprland's `no_screen_share` only blacks a layer out).

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::capture::{SharedCaptureTelemetry, scrawl_log};
use crate::model::{Stroke, TargetInfo};
use crate::sketch::{FreehandOptions, freehand_outline};

const RED: [u8; 4] = [255, 59, 48, 255];
const GREEN: [u8; 4] = [52, 199, 89, 255];
const GLOW_MS: u64 = 900;

pub struct Overlay {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Overlay {
    pub fn start(telemetry: Arc<SharedCaptureTelemetry>, target: &TargetInfo) -> Result<Self> {
        Self::start_with(telemetry, target, RED, None)
    }

    /// The green edge flash that says a stop was taken, on its own surface (the recording's
    /// overlay is going away). It runs out by itself.
    pub fn flash_stopped(target: &TargetInfo) {
        let (tx, _rx) = flume::unbounded();
        let quiet = SharedCaptureTelemetry::new(tx);
        let target = target.clone();
        std::thread::spawn(move || {
            if let Ok(o) = Self::start_with(quiet, &target, GREEN, Some(Duration::from_millis(GLOW_MS + 100))) {
                o.wait();
            }
        });
    }

    fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn start_with(
        telemetry: Arc<SharedCaptureTelemetry>,
        target: &TargetInfo,
        glow: [u8; 4],
        life: Option<Duration>,
    ) -> Result<Self> {
        if std::env::var_os("SCRAWL_NO_OVERLAY").is_some() {
            bail!("disabled by SCRAWL_NO_OVERLAY");
        }
        let monitors = crate::linux::hypr::monitors()?;
        let mon = monitors
            .iter()
            .find(|m| m.id.max(0) as u32 == target.display_id)
            .or_else(|| {
                monitors.iter().find(|m| {
                    m.contains(
                        target.frame.x + target.frame.width / 2.0,
                        target.frame.y + target.frame.height / 2.0,
                    )
                })
            })
            .context("no monitor for the overlay")?
            .clone();
        let target = target.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("scrawl-overlay".into())
            .spawn(move || {
                if let Err(e) = run(&telemetry, &target, &mon, &thread_stop, &ready_tx, glow, life) {
                    scrawl_log(format!("overlay: {e:#}"));
                    let _ = ready_tx.send(Err(e.to_string()));
                }
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => {
                stop.store(true, Ordering::Release);
                bail!("overlay did not configure in time")
            }
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    configured: Option<(u32, u32)>,
    closed: bool,
    busy: [bool; 2],
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer_shell = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_output" if version >= 4 => {
                    let idx = st.outputs.len();
                    let out = reg.bind(name, 4, qh, idx);
                    st.outputs.push((out, None));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = ev
            && let Some(o) = st.outputs.get_mut(*idx)
        {
            o.1 = Some(name);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        st: &mut Self,
        ls: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                ls.ack_configure(serial);
                st.configured = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => st.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for State {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        ev: wl_buffer::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = ev {
            st.busy[*idx] = false;
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore ZwlrLayerShellV1);

/// Reads whatever events are waiting without blocking.
fn pump(conn: &Connection, q: &mut wayland_client::EventQueue<State>, st: &mut State) -> Result<()> {
    conn.flush()?;
    if let Some(guard) = conn.prepare_read() {
        let fd = guard.connection_fd();
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
            let _ = guard.read();
        }
    }
    q.dispatch_pending(st)?;
    Ok(())
}

struct Shm {
    ptr: *mut u8,
    len: usize,
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

fn run(
    telemetry: &SharedCaptureTelemetry,
    target: &TargetInfo,
    mon: &crate::linux::hypr::Monitor,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<std::result::Result<(), String>>,
    glow: [u8; 4],
    life: Option<Duration>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::default();
    q.roundtrip(&mut st)?;
    q.roundtrip(&mut st)?;
    let compositor = st.compositor.clone().context("no wl_compositor")?;
    let shm = st.shm.clone().context("no wl_shm")?;
    let layer_shell = st.layer_shell.clone().context("no zwlr_layer_shell_v1")?;
    let output = st
        .outputs
        .iter()
        .find(|(_, n)| n.as_deref() == Some(mon.name.as_str()))
        .map(|(o, _)| o.clone());

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        output.as_ref(),
        Layer::Overlay,
        "scrawl-overlay".into(),
        &qh,
        (),
    );
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();
    let bs = mon.scale.ceil().max(1.0) as i32;
    surface.set_buffer_scale(bs);
    surface.commit();
    let deadline = Instant::now() + Duration::from_secs(2);
    while st.configured.is_none() {
        q.roundtrip(&mut st)?;
        if Instant::now() > deadline {
            bail!("layer surface was never configured");
        }
    }
    let (lw, lh) = st.configured.unwrap();
    let (lw, lh) = if lw == 0 || lh == 0 {
        let (w, h) = mon.logical_size();
        (w as u32, h as u32)
    } else {
        (lw, lh)
    };
    let (bw, bh) = (lw * bs as u32, lh * bs as u32);
    let stride = bw * 4;
    let frame_len = (stride * bh) as usize;
    let fd = unsafe { libc::memfd_create(c"scrawl-overlay".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), (frame_len * 2) as i64) } != 0 {
        bail!("ftruncate failed");
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            frame_len * 2,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        bail!("mmap failed");
    }
    let map = Shm {
        ptr: ptr.cast(),
        len: frame_len * 2,
    };
    let pool = shm.create_pool(fd.as_fd(), (frame_len * 2) as i32, &qh, ());
    let buffers = [0usize, 1].map(|i| {
        pool.create_buffer(
            (i * frame_len) as i32,
            bw as i32,
            bh as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            &qh,
            i,
        )
    });
    let _ = ready.send(Ok(()));
    scrawl_log(format!(
        "overlay on {} ({}x{} px, buffer scale {bs})",
        mon.name, bw, bh
    ));

    // Geometry: strokes are normalized to the capture frame; the overlay covers the monitor.
    let px_per_lp = bs as f32;
    let fr = target.frame;
    let to_px = |nx: f32, ny: f32| -> [f32; 2] {
        [
            ((fr.x + f64::from(nx) * fr.width - mon.x) as f32) * px_per_lp,
            ((fr.y + f64::from(ny) * fr.height - mon.y) as f32) * px_per_lp,
        ]
    };
    // Upstream's keyframe ink size, at the size the video (and so the keyframes) has.
    let (vw, vh) = (
        (fr.width * fr.scale) as f32,
        (fr.height * fr.scale) as f32,
    );
    let k = (vw.min(vh) / 800.0).max(vw.max(vh) / 1280.0).clamp(1.0, 2.0);
    let size_video = (vw.min(vh) * 0.0065).clamp(4.2, 6.0) * k;
    let size = size_video * px_per_lp / fr.scale.max(1.0) as f32;

    let started = Instant::now();
    let mut last_gen = u64::MAX;
    let mut glow_done = false;
    let mut was_fading = false;
    let mut pixmap = tiny_skia::Pixmap::new(bw, bh).context("pixmap")?;
    while !stop.load(Ordering::Acquire) && !st.closed && life.is_none_or(|l| started.elapsed() < l) {
        pump(&conn, &mut q, &mut st)?;
        let generation = telemetry.ink_generation.load(Ordering::Acquire);
        let glow_t = started.elapsed().as_millis() as u64;
        let glowing = glow_t < GLOW_MS;
        let now_ms = telemetry.t_of_mono(crate::capture::mono_us());
        let (strokes, live) = telemetry.ink_snapshot();
        let fading: Vec<(&Stroke, f32)> = strokes
            .iter()
            .filter_map(|(s, released)| fade_alpha(now_ms.saturating_sub(*released)).map(|a| (s, a)))
            .collect();
        // Keep drawing while a stroke fades, and once more after the last one has gone.
        let animating = !fading.is_empty() || was_fading;
        if generation != last_gen || glowing || !glow_done || animating {
            let Some(i) = (0..2).find(|&i| !st.busy[i]) else {
                std::thread::sleep(Duration::from_millis(4));
                continue;
            };
            pixmap.fill(tiny_skia::Color::TRANSPARENT);
            if glowing {
                draw_glow(&mut pixmap, glow, 1.0 - glow_t as f32 / GLOW_MS as f32, px_per_lp);
            }
            for (s, alpha) in fading.iter().copied().chain(live.iter().map(|s| (s, 1.0))) {
                draw_stroke(&mut pixmap, s, size, alpha, &to_px);
            }
            let dst = unsafe { std::slice::from_raw_parts_mut(map.ptr.add(i * frame_len), frame_len) };
            for (d, s) in dst.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
                // tiny-skia: premultiplied RGBA; wl_shm ARGB8888 (little endian): B G R A.
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
            surface.attach(Some(&buffers[i]), 0, 0);
            surface.damage_buffer(0, 0, bw as i32, bh as i32);
            surface.commit();
            st.busy[i] = true;
            last_gen = generation;
            glow_done = !glowing;
            was_fading = !fading.is_empty();
        }
        std::thread::sleep(Duration::from_millis(12));
    }
    layer.destroy();
    surface.destroy();
    for b in &buffers {
        b.destroy();
    }
    pool.destroy();
    let _ = conn.flush();
    drop(map);
    Ok(())
}

fn paint(color: [u8; 4]) -> tiny_skia::Paint<'static> {
    let mut p = tiny_skia::Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], color[3]);
    p.anti_alias = true;
    p
}

/// Upstream's live-ink fade: a finished stroke holds for 650 ms, then fades out over 650 ms
/// (quadratic). `None` once it has gone. The bundle keeps every stroke; only the screen lets go.
fn fade_alpha(age_ms: u64) -> Option<f32> {
    match age_ms {
        0..=650 => Some(1.0),
        651..=1_299 => {
            let t = (age_ms - 650) as f32 / 650.0;
            Some((1.0 - t) * (1.0 - t))
        }
        _ => None,
    }
}

fn draw_stroke(pm: &mut tiny_skia::Pixmap, s: &Stroke, size: f32, alpha: f32, to_px: &impl Fn(f32, f32) -> [f32; 2]) {
    if s.laser {
        return;
    }
    let pts: Vec<[f32; 2]> = s.timed_points().into_iter().map(|(x, y, _)| to_px(x, y)).collect();
    if pts.is_empty() {
        return;
    }
    let outline = freehand_outline(
        &pts,
        &FreehandOptions {
            size,
            ..Default::default()
        },
    );
    let mut pb = tiny_skia::PathBuilder::new();
    let mut it = outline.iter();
    let Some(first) = it.next() else { return };
    pb.move_to(first[0], first[1]);
    for p in it {
        pb.line_to(p[0], p[1]);
    }
    pb.close();
    if let Some(path) = pb.finish() {
        let [r, g, b, a] = RED;
        let color = [r, g, b, (f32::from(a) * alpha).round() as u8];
        pm.fill_path(&path, &paint(color), tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
    }
}

/// A red glow along the edges that fades out: "recording started".
fn draw_glow(pm: &mut tiny_skia::Pixmap, color: [u8; 4], strength: f32, scale: f32) {
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    let depth = 34.0 * scale;
    let a = (strength.clamp(0.0, 1.0) * 0.75 * 255.0) as u8;
    let edge = tiny_skia::Color::from_rgba8(color[0], color[1], color[2], a);
    let clear = tiny_skia::Color::from_rgba8(color[0], color[1], color[2], 0);
    let rects = [
        (tiny_skia::Rect::from_xywh(0.0, 0.0, w, depth), (0.0, 0.0), (0.0, depth)),
        (tiny_skia::Rect::from_xywh(0.0, h - depth, w, depth), (0.0, h), (0.0, h - depth)),
        (tiny_skia::Rect::from_xywh(0.0, 0.0, depth, h), (0.0, 0.0), (depth, 0.0)),
        (tiny_skia::Rect::from_xywh(w - depth, 0.0, depth, h), (w, 0.0), (w - depth, 0.0)),
    ];
    for (rect, from, to) in rects {
        let (Some(rect), Some(shader)) = (
            rect,
            tiny_skia::LinearGradient::new(
                tiny_skia::Point::from_xy(from.0, from.1),
                tiny_skia::Point::from_xy(to.0, to.1),
                vec![
                    tiny_skia::GradientStop::new(0.0, edge),
                    tiny_skia::GradientStop::new(1.0, clear),
                ],
                tiny_skia::SpreadMode::Pad,
                tiny_skia::Transform::identity(),
            ),
        ) else {
            continue;
        };
        let p = tiny_skia::Paint {
            shader,
            anti_alias: false,
            ..Default::default()
        };
        pm.fill_rect(rect, &p, tiny_skia::Transform::identity(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::fade_alpha;

    #[test]
    fn ink_holds_then_fades_like_upstream() {
        assert_eq!(fade_alpha(0), Some(1.0));
        assert_eq!(fade_alpha(650), Some(1.0));
        assert!((fade_alpha(975).unwrap() - 0.25).abs() < 0.01);
        assert_eq!(fade_alpha(1_300), None);
    }
}
