# Omarchy fork: findings

These are measured on wubulon (Omarchy 4.0.4, Hyprland 0.56.2, gsr 6.1.0, AMD RX 6600 + NVIDIA GTX 1660 SUPER), 2026-10-19.
✅ means measured or read in source. 🟡 means inferred.

## E1: in-compositor Lua as the input source ✅ works

Method: a nested Hyprland 0.56.2 (`experiments/e1/hl.lua`) and the `experiments/vinput` virtual pointer/keyboard injector, plus a
GTK4 client that logs what the app receives (`client.py`). The Lua probe (`inject.lua`) was loaded with `hyprctl eval 'dofile(...)'`.

- `hl.bind("mouse:272"|273|274, fn, {non_consuming, ignore_mods, transparent, submap_universal})` fires on press, and with `release = true`
  on release. `hl.get_cursor_pos()` gives exact layout px.
- Non-consuming: the GTK client still received every press, release, scroll and key.
- `ignore_mods` works. Shift+click and right-click were both observed.
- `hl.on("input.keyboard.key", fn(keycode, time_ms, state))` passes the xkb keycode (evdev + 8), the device event time in ms,
  and state 1 or 0.
- Latency from inject to Lua to line-buffered file to reader: **0.3–1.1 ms**.
- Wheel binds (`mouse_up/down/left/right`) fire, but Hyprland's `binds:scroll_event_delay` (300 ms by default) throttles them. A fast
  scroll burst yields about one event per 300 ms.
- `hl.is_key_down()` inside the key handler reports the state *before* the event. Track modifiers from the key events.
- Omarchy's `SUPER + mouse:272` window-drag bind (`mouse = true`) coexists. Our observer binds still log the down and up.
- `hyprctl reload` wipes the whole Lua state, including globals and binds. Re-inject on socket2 `configreloaded`.

### Hyprland 0.56.2 crash bug (avoid it)
Bind two keybinds on the **same key** (press plus release), `:remove()` one of them, then touch the other's Lua handle.
The compositor gets a **SIGSEGV** in `CLuaKeybind` (crash report kept outside the repo). `:remove()` and `hl.unbind(key)` remove *every*
bind on that key and leave the other handles dangling.
Safe: `set_enabled(false/true)`, `EventSubscription:remove()`, removing binds on distinct keys, and GC of dangling handles.
**Rule: never `:remove()` our binds. Inject once per Lua state (guarded by a global) and toggle with `set_enabled`.**

## §5 #1: hiding the overlay from the recording ✅ impossible as hoped
- gsr's kms capture reads the composited scanout, so every layer is in it.
- `no_screen_share` on a layer does **not** hide it from screencopy or portal capture. Hyprland paints a **black box the size of the layer**
  instead (`src/managers/screenshare/ScreenshareFrame.cpp:241-255`, v0.56.2). On a fullscreen overlay that blacks out the whole capture.
- Decision: live ink is in the video. The overlay draws ink with upstream's own geometry and colour (`sketch::freehand_outline`),
  so the ink that keyframes and export re-render lands on top of it.

## §5 #5: gsr capture clock ✅
4 s capture of HDMI-A-2, `-write-first-frame-ts yes -ipc sock`, stopped with IPC `{"name":"stop"}`.
- `.ts` = `monotonic_us realtime_us` of the first frame, here 491.8 ms after spawn.
- First frame + video duration (3.517 s) matches the time the stop was sent **to within 1 ms**. The `.ts` is the first frame's capture time.
- The IPC stop replies after the file is written (9 ms here), with the saved path.
- Both streams have `start_time=0`. The mic track was 110 ms longer than the video. A/V skew is still unmeasured, and a beep test is pending.
- gsr logs one `failed to write frame index 1 to muxer` at start. It is harmless; the duration is correct.

## Other corrections to the packet
- wubulon has both an AMD RX 6600 and an NVIDIA GTX 1660 SUPER, not AMD only.
- gsr `-w focused` is X11-only, like `-w <window_id>`.
- The CUA source is local at `~/src/cua` (MIT).
- `hl.on` returns an `HL.EventSubscription` with `:remove()`. Stubs are in `/usr/share/hypr/stubs/hl.meta.lua`.
- `AQ_BACKENDS=headless` aborts (`CBackend::create() failed`), so it can't be used for invisible tests. Nested runs as a window on a silent
  workspace (`hl.exec_cmd(cmd, {workspace = "19 silent", no_initial_focus = true})`). A hidden nested instance does not render, so
  screencopy (`grim`) against it hangs.
- exe.dev plan = 2 vCPU / 8 GB total across all VMs, so a new VM can't make builds faster. Use the `warmap` VM for compile checks.

## Fork shape (cherry-pick friendly)
- Irreducibly Mac modules are swapped on Linux with `#[cfg_attr(target_os = "linux", path = "linux/…")]` in `main.rs`, so upstream files
  stay byte-identical: capture, helper, ui, settings, sparkle, laser_overlay, icons.
- Nearly portable modules get a handful of `cfg` lines (windows, retention, handoff's non-mac stubs). Schema types are never copied.
- `helper.rs` becomes an ffmpeg/ffprobe/whisper-cli shim with the same signatures. Its `transcribe` fills the default ("apple") engine slot.
