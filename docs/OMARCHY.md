# Hyprscrawl: Scrawl on Hyprland (Omarchy)

This fork runs Scrawl on stock Omarchy 4 (Hyprland ≥ 0.55 with a Lua config, Wayland). It produces the same
`.scrawl` bundles as macOS Scrawl: `contact-sheet.png`, `frames/frame-XX.png`, `summary.md`, `scrawl.json`,
`recording.mp4` and `audio.wav`.
Setup needs no root, no `input` group, no Hyprland plugin and no edits to your config.

## Use

| | |
|---|---|
| **Alt+Shift+S** (tap) | Record the screen under the pointer. A red edge glow flashes as the start cue. Tap again to finish. |
| **Alt+Shift+S** (hold) | Pick what to record: drag an area, or click a window. Esc cancels. |
| **Hold Shift + move** | Draw red ink while recording. A stroke fades from the screen 1.3 s after you let go (the bundle keeps it). Clicks still go to your apps. |
| **Ctrl+Z** | Undo the last stroke while recording. |
| **Esc** | Cancel the recording; nothing is saved. Esc is only taken while recording. |
| Finish | The bundle is built (transcript, keyframes, contact sheet) and its summary is copied to the clipboard. A "Scrawl saved" notification shows the contact sheet. |
| **Alt+Shift+R** | Open the latest capture in the review window: contact sheet / keyframes, transcript and timeline. Clicking the "saved" notification does the same. |
| **Alt+Shift+V** | Paste the latest capture (contact sheet, then summary) into the focused window. |
| `scrawl preview [latest\|BUNDLE]` | The review window from a terminal. |
| `scrawl setup launcher` | Adds "Scrawl: Review Latest Capture" and "Scrawl: Record" to the app launcher (Super+Space). |
| `scrawl doctor` | Lists what is missing and how to install it, and the shortcuts in use. |

Scrawl's notifications go through Omarchy as user-action confirmations (like "Screenshot saved"),
so they still show while Do Not Disturb is on. To open each finished capture's review window
without clicking anything, set `"linuxReviewOnFinish": true` in `~/.config/scrawl/config.json`.

The shortcuts are *runtime* Hyprland binds added by the daemon. No file is edited, and
`hyprctl reload` removes them until the daemon adds them back. For permanent binds, run
`scrawl hyprland-snippet` and paste its lines into `~/.config/hypr/bindings.lua`; the daemon then
leaves those chords to them instead of binding them a second time. To change or turn off
a shortcut, set `linuxHotkey`, `linuxPasteHotkey` or `linuxReviewHotkey` (for example
`"SUPER + SHIFT + R"`, or `"none"`) in `~/.config/scrawl/config.json` and restart the daemon
(`systemctl --user restart scrawl`): the old chord is switched off. A chord that something else
already uses is left alone (see the log).

Captures go to `~/.local/share/scrawl/captures/` (`SCRAWL_CAPTURES_DIR`). The log is at `~/.local/state/scrawl/scrawl.log`.

## Install

Requirements, all in Omarchy's base set: `gpu-screen-recorder ffmpeg wl-clipboard wtype slurp`, plus Vulkan for
the review window.

Build and install, then run it as a service:

```sh
cargo build --release
install -m755 target/release/scrawl ~/.local/bin/scrawl
scrawl setup service     # systemd user service tied to the Hyprland session; restarts if it crashes
scrawl doctor
```

The service starts with each Hyprland session and adds the shortcuts. `systemctl --user status scrawl`
and `journalctl --user -u scrawl` show how it is doing. Without the service, `scrawl toggle` starts
the daemon on demand, but the shortcuts only exist once the daemon is running.

Transcription uses whisper.cpp:

```sh
omarchy-pkg-add whisper-cpp     # provides whisper-cli
scrawl setup transcription      # downloads ggml-base.en (148 MB), after asking
```

Or use the transcription that comes with a ChatGPT subscription (the same endpoint ChatGPT and Codex
voice input use), signed in through the Codex CLI:

```sh
codex login                             # once, if you haven't
scrawl setup transcription chatgpt      # back to local only: scrawl setup transcription local
```

The audio is uploaded to OpenAI. The service returns text without word times, so when whisper.cpp is
installed it runs alongside and the ChatGPT words take whisper's timings (33 ms median word-start error
on a ground-truth clip). Without whisper, the words are spread over the detected speech and marked
estimated, so spoken cues don't cut keyframes. If ChatGPT fails, whisper.cpp transcribes instead.
`scrawl reprocess` / `import` with `--transcriber apple` (or `hex`) stay local for that run: nothing is uploaded.

If you already set up Voxtype dictation, its `~/.local/share/voxtype/models/ggml-*.bin` is reused (read-only).
`SCRAWL_WHISPER_CLI` / `SCRAWL_WHISPER_MODEL` override the lookup, and a `whisper-cli` next to the `scrawl`
binary is used first.

## How it works

- **Video and mic**: `gpu-screen-recorder` (kms, hardware encode, CPU fallback), driven directly with
  `-write-first-frame-ts` (that gives the capture clock, `videoOffsetMs`) and `-ipc` (a clean stop). Omarchy's
  bar shows its recording indicator. Omarchy's own screen-record stop also ends a Scrawl capture cleanly.
- **Input**: Lua injected with `hyprctl eval` adds non-consuming mouse and wheel binds and a
  `input.keyboard.key` hook. Only modifiers, and keys pressed with Ctrl/Alt/Super, leave the compositor, so
  typing is never logged. The cursor path comes from Hyprland's socket at 125 Hz, and focus changes and
  reloads from its event socket. Hyprland's `binds:scroll_event_delay` (300 ms) limits how many scroll
  events are seen.
- **Ink overlay**: a click-through `wlr-layer-shell` surface drawn with upstream's stroke geometry. It is
  part of the video, because no compositor capture can leave a layer out (see `FINDINGS.md`).
- **Transcription**: whisper.cpp with DTW token timings. Word starts land within about ±130 ms on a
  ground-truth clip, so spoken cues ("look", "here", "now") can still place keyframes.
- **Everything after capture is upstream's code**: keyframes, autocrop, contact sheet, summary, export
  plan, routing and delivery.

## Not yet on Linux

- The menu-bar capsule and its motion lab.
- The Settings window. Use `config.json`.
- iPhone capture.
- Sparkle updates.
- Video export rendering (`scrawl export` builds the plan, but its renderer is the AVFoundation helper).
- Multi-MIME clipboard. The text summary is copied; `scrawl paste` pastes image then text.
- The capsule ("island") with its mode switcher and Paste / Inspect chips: notifications and the
  shortcuts above stand in for it.
- Sending dev-site captures to their OpenCode session.

## Following upstream

The fork keeps upstream's files byte-identical where it can:

- macOS-only modules are swapped with `#[cfg_attr(target_os = "linux", path = "linux/…")]` in `src/main.rs`:
  capture, helper, ui, settings, sparkle, laser_overlay, icons.
- Everything Linux lives in `src/linux/`.
- Upstream files are touched in only a few places, about 35 lines: `src/main.rs` (module attributes and the
  Linux command hook), `src/windows.rs` (two `cfg`s and an `include!`), `src/retention.rs` (one `cfg`),
  `src/handoff.rs` (two non-mac stubs), and `Cargo.toml` (a Linux dependency table).

```sh
git fetch upstream
git merge upstream/main      # conflicts, if any, are in the lines listed above
cargo test                   # upstream's tests run on Linux too
```

If upstream changes a function that a swapped module provides (for example a new `capture::` call from
`main.rs`), the Linux build fails to compile until the matching function is added in `src/linux/`.
