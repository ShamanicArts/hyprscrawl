//! Synthetic input for a *nested* Hyprland: reads a script on stdin and drives
//! zwlr_virtual_pointer_v1 + zwp_virtual_keyboard_v1 on $WAYLAND_DISPLAY.
//! Each action prints `<CLOCK_MONOTONIC us> <action>` so a test can compare
//! injection times with what an observer recorded.
//!
//! Script lines:
//!   move X Y        absolute position in layout px (needs `extent W H` first)
//!   extent W H      output extent for absolute motion
//!   rel DX DY       relative motion
//!   down B | up B   button (272 left, 273 right, 274 middle)
//!   wheel DY [DX]   scroll (discrete steps of 15)
//!   key CODE 1|0    evdev keycode press/release (e.g. 42 = LeftShift, 29 = LeftCtrl, 44 = Z)
//!   mods DEP        set depressed modifier mask (1 shift, 4 ctrl, 8 alt, 64 super)
//!   sleep MS
use std::io::{BufRead, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    vptr_mgr: Option<ZwlrVirtualPointerManagerV1>,
    vkbd_mgr: Option<ZwpVirtualKeyboardManagerV1>,
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
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_seat" if st.seat.is_none() => {
                    st.seat = Some(reg.bind(name, version.min(7), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    st.vptr_mgr = Some(reg.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    st.vkbd_mgr = Some(reg.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

fn mono_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

fn keymap_fd() -> (OwnedFd, u32) {
    let out = std::process::Command::new("xkbcli")
        .args(["compile-keymap", "--layout", "us"])
        .output()
        .expect("xkbcli compile-keymap");
    let mut km = out.stdout;
    km.push(0);
    let fd = unsafe { libc::memfd_create(c"vinput-keymap".as_ptr(), 0) };
    assert!(fd >= 0);
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(&km).unwrap();
    (OwnedFd::from(f), km.len() as u32)
}

fn main() {
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::default();
    q.roundtrip(&mut st).unwrap();
    let seat = st.seat.clone().expect("no wl_seat");
    let vptr = st
        .vptr_mgr
        .as_ref()
        .expect("no zwlr_virtual_pointer_manager_v1")
        .create_virtual_pointer(Some(&seat), &qh, ());
    let vkbd = st
        .vkbd_mgr
        .as_ref()
        .expect("no zwp_virtual_keyboard_manager_v1")
        .create_virtual_keyboard(&seat, &qh, ());
    let (fd, size) = keymap_fd();
    vkbd.keymap(1, fd.as_fd(), size); // 1 = XKB_V1
    q.roundtrip(&mut st).unwrap();

    let t0 = std::time::Instant::now();
    let ms = || t0.elapsed().as_millis() as u32;
    let (mut ew, mut eh) = (1920u32, 1080u32);
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.is_empty() || p[0].starts_with('#') {
            continue;
        }
        let n = |i: usize| p.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        match p[0] {
            "extent" => {
                ew = n(1) as u32;
                eh = n(2) as u32;
                continue;
            }
            "sleep" => {
                conn.flush().ok();
                std::thread::sleep(std::time::Duration::from_millis(n(1) as u64));
                continue;
            }
            "move" => {
                vptr.motion_absolute(ms(), n(1) as u32, n(2) as u32, ew, eh);
                vptr.frame();
            }
            "rel" => {
                vptr.motion(ms(), n(1), n(2));
                vptr.frame();
            }
            "down" | "up" => {
                let state = if p[0] == "down" {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                vptr.button(ms(), n(1) as u32, state);
                vptr.frame();
            }
            "wheel" => {
                let t = ms();
                vptr.axis_source(wl_pointer::AxisSource::Wheel);
                if n(1) != 0.0 {
                    vptr.axis_discrete(t, wl_pointer::Axis::VerticalScroll, n(1) * 15.0, n(1) as i32);
                }
                if n(2) != 0.0 {
                    vptr.axis_discrete(t, wl_pointer::Axis::HorizontalScroll, n(2) * 15.0, n(2) as i32);
                }
                vptr.frame();
            }
            "key" => vkbd.key(ms(), n(1) as u32, n(2) as u32),
            "mods" => vkbd.modifiers(n(1) as u32, 0, 0, 0),
            other => {
                eprintln!("unknown: {other}");
                continue;
            }
        }
        conn.flush().ok();
        writeln!(stdout.lock(), "{} {}", mono_us(), line).ok();
    }
    conn.flush().ok();
    q.roundtrip(&mut st).ok();
}
