//! Linux stand-in for `settings.rs` (the SwiftUI Settings window). Shortcuts are Hyprland
//! binds here (`scrawl hyprland-snippet`).

/// No Settings window events on Linux.
#[derive(Debug)]
pub enum Event {}

pub fn paste_label() -> String {
    "Alt+Shift+V".to_owned()
}
