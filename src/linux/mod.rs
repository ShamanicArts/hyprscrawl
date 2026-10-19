//! Linux (Omarchy / Hyprland) platform code. The modules that replace upstream's macOS-only
//! files are wired in from `main.rs` with `#[cfg_attr(target_os = "linux", path = …)]`;
//! everything else Linux-specific lives here.

pub mod cli;
pub mod daemon;
pub mod desktop;
pub mod events;
pub mod gsr;
pub mod hypr;
pub mod overlay;
pub mod whisper;
