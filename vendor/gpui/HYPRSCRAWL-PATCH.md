# gpui 0.2.2, patched for Hyprscrawl

Unmodified crates.io `gpui` 0.2.2 (Apache-2.0, see LICENSE-APACHE) except:

- `examples/` and `Cargo.lock` left out, and the `[[example]]` targets removed from `Cargo.toml`.
- `src/platform/linux/wayland/client.rs`: bind the **first** `wl_seat` and ignore seats announced
  later. Upstream binds the last one (`// TODO: Multi seat support`), so on Hyprland with the CUA
  plugin, which advertises `Hyprland`, `Cua-Agent` and `Cua-Agent-2` in that order, GPUI windows
  listened to an automation seat and never received the user's clicks or keys.

Drop this directory and the `[patch.crates-io]` entry once GPUI handles multiple seats.
