// Linux window-server pieces for `windows.rs`, pulled in with `include!` so upstream's
// `WindowLogger`, `run`, `snapshot` and `filter_windows` run unchanged on Hyprland.

/// Lets `snapshot`'s `objc2::rc::autoreleasepool(|_| …)` compile: a local module shadows the
/// (absent) crate.
#[allow(dead_code)]
mod objc2 {
    pub mod rc {
        pub fn autoreleasepool<R>(f: impl FnOnce(()) -> R) -> R {
            f(())
        }
    }
}

/// The visible windows (front to back) from `hyprctl clients`, in global layout px.
pub fn raw_windows() -> Vec<RawWindow> {
    crate::linux::hypr::visible_clients()
        .unwrap_or_default()
        .into_iter()
        .map(|c| RawWindow {
            id: c.id(),
            pid: c.pid,
            owner: c.class.clone(),
            title: c.title.clone(),
            bounds: [c.at[0], c.at[1], c.size[0], c.size[1]],
            layer: 0,
            alpha: 1.0,
        })
        .collect()
}

/// Every Hyprland client is a regular app window; its class stands in for a bundle id.
fn app_meta_of(_pid: i32) -> AppMeta {
    AppMeta {
        bundle_id: None,
        regular: true,
    }
}

/// No Dock on Hyprland.
pub fn dock_rect(_frame: TargetFrame) -> Option<[f64; 4]> {
    None
}
