//! The review window on Linux (`scrawl preview [bundle|latest]`, and after a capture): upstream's
//! `BundleReviewWindow` layout and widgets (contact sheet / keyframes stage, transcript and
//! timeline sidebar), ported from `ui.rs` without the macOS coordinator. Actions go through the
//! Linux daemon and desktop helpers.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Application, Bounds, BoxShadow, Context, Div, FocusHandle, FontWeight, Image,
    ImageFormat, IntoElement, KeyDownEvent, Render, SharedString, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, div, img, point, prelude::*, px, relative, rgb, rgba,
    size,
};

use crate::model::{ScrawlBundle, format_clock};
use crate::sketch::timeline;

const SIDEBAR_BG: u32 = 0x111111;
const CANVAS_BG: u32 = 0x161616;
const SURFACE_BG: u32 = 0x1d1d1d;
const SURFACE_HOVER: u32 = 0x252525;
const SURFACE_SELECTED: u32 = 0x2e2e2e;
const LINE_COLOR: u32 = 0x2a2a2a;
const TEXT_COLOR: u32 = 0xf2f2f2;
const TEXT_SOFT: u32 = 0xd0d0d0;
const MUTED_COLOR: u32 = 0x969696;
const ACCENT_COLOR: u32 = 0x3b5cf6;
const ACCENT_HOVER: u32 = 0x4b6bff;
const INK_RED: u32 = 0xff3b30;

pub const APP_ID: &str = "dev.scrawl.Scrawl";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    ContactSheet,
    Keyframes,
}

pub struct ReviewWindow {
    dir: PathBuf,
    bundle: ScrawlBundle,
    focus: FocusHandle,
    tab: Tab,
    selected: usize,
    status: Option<String>,
    sheet: Option<Arc<Image>>,
    frames: Vec<Option<Arc<Image>>>,
}

fn load_png(path: &std::path::Path) -> Option<Arc<Image>> {
    let bytes = std::fs::read(path).ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
}

impl ReviewWindow {
    fn new(dir: PathBuf, bundle: ScrawlBundle, focus: FocusHandle) -> Self {
        let sheet = load_png(&dir.join(&bundle.contact_sheet));
        let frames = bundle
            .keyframes
            .iter()
            .map(|kf| load_png(&dir.join(&kf.file)))
            .collect();
        Self {
            dir,
            bundle,
            focus,
            tab: Tab::ContactSheet,
            selected: 0,
            status: None,
            sheet,
            frames,
        }
    }

    fn render_contact_sheet_stage(&self, stage_w: f32) -> AnyElement {
        if let Some(ref image) = self.sheet {
            div()
                .w_full()
                .min_w_0()
                .flex()
                .justify_center()
                .child(
                    img(Arc::clone(image))
                        .w(px(stage_w))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(rgb(LINE_COLOR)),
                )
                .into_any_element()
        } else {
            div()
                .p_6()
                .text_color(rgb(MUTED_COLOR))
                .child("Contact sheet image unavailable.")
                .into_any_element()
        }
    }

    fn render_keyframes_stage(&self, stage_w: f32, cx: &mut Context<Self>) -> AnyElement {
        // Card: border 1 + pad 8 around the frame, so R_card 12 = R_frame 3 + 9.
        let card_img_w = (stage_w - 18.0).max(240.0);
        let cards = self
            .bundle
            .keyframes
            .iter()
            .enumerate()
            .map(|(idx, kf)| {
                let is_selected = idx == self.selected;
                let frame_img = self.frames.get(idx).and_then(Clone::clone);
                div()
                    .id(("kf-card", idx))
                    .p(px(8.0))
                    .rounded(px(12.0))
                    .bg(rgb(SURFACE_BG))
                    .border_1()
                    .border_color(if is_selected {
                        rgb(ACCENT_COLOR)
                    } else {
                        rgb(LINE_COLOR)
                    })
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .when(!is_selected, |d| d.hover(|s| s.border_color(rgb(0x3a3a3a))))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = idx;
                        cx.notify();
                    }))
                    .when_some(frame_img, |d, image| {
                        d.child(img(image).w(px(card_img_w)).rounded(px(3.0)))
                    })
                    .child(
                        div()
                            .px(px(4.0))
                            .pb(px(2.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(12.5))
                            .child(
                                div()
                                    .h(px(20.0))
                                    .px(px(7.0))
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .rounded(px(10.0))
                                    .bg(rgb(CANVAS_BG))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(11.5))
                                    .text_color(rgb(TEXT_COLOR))
                                    .child(format!("#{} · {}", kf.index, format_clock(kf.t))),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(TEXT_SOFT))
                                    .child(kf.event.clone()),
                            ),
                    )
                    .when_some(kf.said.clone(), |d, said| {
                        d.child(
                            div()
                                .px(px(4.0))
                                .pb(px(2.0))
                                .text_size(px(12.0))
                                .line_height(relative(1.4))
                                .text_color(rgb(MUTED_COLOR))
                                .child(format!("“{said}”")),
                        )
                    })
            })
            .collect::<Vec<_>>();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .children(cards)
            .into_any_element()
    }
}

impl Render for ReviewWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stage_w = (f32::from(window.bounds().size.width) - 320.0 - 44.0).max(320.0);
        let bundle = &self.bundle;
        let target_label = match (&bundle.target.app, &bundle.target.window_title) {
            (Some(app), Some(win)) if !win.is_empty() && win != app => format!("{app} — “{win}”"),
            (Some(app), _) => app.clone(),
            _ => format!("Display {}", bundle.target.display_id),
        };
        let lines = timeline(bundle);
        let header_info = format!(
            "{} · {} · {} {}",
            target_label,
            format_clock(bundle.duration_ms),
            bundle.keyframes.len(),
            if bundle.keyframes.len() == 1 { "keyframe" } else { "keyframes" }
        );
        let transcript = bundle.transcript.text.trim().to_owned();
        let tab = self.tab;
        let copied = self.status.as_deref() == Some("Copied ✓");
        let status = self.status.clone();
        let stage = match tab {
            Tab::ContactSheet => self.render_contact_sheet_stage(stage_w),
            Tab::Keyframes => self.render_keyframes_stage(stage_w, cx),
        };

        div()
            .id("scrawl-review-root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = event.keystroke.modifiers;
                if key == "escape" || (mods.control && key.eq_ignore_ascii_case("w")) {
                    window.remove_window();
                } else if mods.control && key.eq_ignore_ascii_case("c") {
                    this.copy(cx);
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(CANVAS_BG))
            .text_color(rgb(TEXT_COLOR))
            .child(
                div()
                    .h(px(48.0))
                    .px_4()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .bg(rgb(SIDEBAR_BG))
                    .border_b_1()
                    .border_color(rgb(LINE_COLOR))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                // Segmented control: R_outer 8 = R_tab 6 + border 1 + pad 1.
                                div()
                                    .flex_none()
                                    .h(px(28.0))
                                    .p(px(1.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .rounded(px(8.0))
                                    .bg(rgb(CANVAS_BG))
                                    .border_1()
                                    .border_color(rgb(LINE_COLOR))
                                    .child(tab_chip("tab-contact-sheet", "Contact Sheet", tab == Tab::ContactSheet).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.tab = Tab::ContactSheet;
                                            cx.notify();
                                        }),
                                    ))
                                    .child(tab_chip("tab-keyframes", "Keyframes", tab == Tab::Keyframes).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.tab = Tab::Keyframes;
                                            cx.notify();
                                        }),
                                    )),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(rgb(MUTED_COLOR))
                                    .child(header_info),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when_some(status.filter(|s| s != "Copied ✓"), |d, s| {
                                d.child(div().text_size(px(12.0)).text_color(rgb(MUTED_COLOR)).child(s))
                            })
                            .child(
                                hint_button("btn-record", "Record", crate::linux::daemon::hotkey(), false).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.status = Some("Recording…".into());
                                        cx.notify();
                                        std::thread::spawn(|| {
                                            let _ = crate::linux::daemon::send("start", true);
                                        });
                                        window.remove_window();
                                    }),
                                ),
                            )
                            .child(action_button("btn-reveal", "Open Folder", false).on_click(cx.listener(
                                |this, _, _, cx| {
                                    let _ = std::process::Command::new("xdg-open").arg(&this.dir).spawn();
                                    this.status = Some("Opened folder".into());
                                    cx.notify();
                                },
                            )))
                            .child(
                                action_button("btn-copy", if copied { "Copied" } else { "Copy" }, true)
                                    .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                            ),
                    ),
            )
            .child(
                // Main workspace: stage on the left, timeline sidebar on the right.
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("review-stage-scroll")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .overflow_x_hidden()
                            .p_5()
                            .child(stage),
                    )
                    .child(
                        div()
                            .id("review-timeline-sidebar")
                            .w(px(320.0))
                            .h_full()
                            .flex_none()
                            .overflow_y_scroll()
                            .overflow_x_hidden()
                            .bg(rgb(SIDEBAR_BG))
                            .border_l_1()
                            .border_color(rgb(LINE_COLOR))
                            .px_4()
                            .py(px(18.0))
                            .flex()
                            .flex_col()
                            .child(sidebar_heading("Transcript"))
                            .child(
                                div()
                                    .p_3()
                                    .rounded(px(8.0))
                                    .bg(rgb(SURFACE_BG))
                                    .border_1()
                                    .border_color(rgb(LINE_COLOR))
                                    .text_size(px(13.0))
                                    .line_height(relative(1.45))
                                    .map(|d| {
                                        if transcript.is_empty() {
                                            d.text_color(rgb(MUTED_COLOR)).child("No speech")
                                        } else {
                                            d.text_color(rgb(TEXT_COLOR)).child(format!("“{transcript}”"))
                                        }
                                    }),
                            )
                            .child(div().mt(px(20.0)).child(sidebar_heading("Timeline")))
                            .child(
                                div()
                                    .mx(px(-8.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.0))
                                    .children(lines.into_iter().enumerate().map(|(i, line)| timeline_row(i, &line))),
                            ),
                    ),
            )
    }
}

impl ReviewWindow {
    fn copy(&mut self, cx: &mut Context<Self>) {
        let res = crate::handoff::copy_bundle_to_clipboard(&self.dir);
        self.status = Some(match res {
            Ok(()) => "Copied ✓".into(),
            Err(e) => format!("Copy failed: {e}"),
        });
        cx.notify();
    }
}

fn tab_chip(id: &'static str, label: &'static str, selected: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(10.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .when(selected, |d| {
            d.bg(rgb(SURFACE_SELECTED)).shadow(vec![BoxShadow {
                color: gpui::hsla(0.0, 0.0, 0.0, 0.25),
                offset: point(px(0.0), px(1.0)),
                blur_radius: px(2.0),
                spread_radius: px(0.0),
            }])
        })
        .text_color(if selected { rgb(TEXT_COLOR) } else { rgb(MUTED_COLOR) })
        .when(!selected, |d| d.cursor_pointer().hover(|s| s.text_color(rgb(TEXT_SOFT))))
        .child(label)
}

fn button_base(id: &'static str, primary: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(12.0))
        .rounded(px(7.0))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .whitespace_nowrap()
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .border_1()
        .border_color(if primary { rgb(ACCENT_COLOR) } else { rgb(LINE_COLOR) })
        .bg(if primary { rgb(ACCENT_COLOR) } else { rgb(SURFACE_BG) })
        .text_color(rgb(TEXT_COLOR))
        .cursor_pointer()
        .hover(move |s| {
            if primary {
                s.bg(rgb(ACCENT_HOVER)).border_color(rgb(ACCENT_HOVER))
            } else {
                s.bg(rgb(SURFACE_HOVER))
            }
        })
}

fn action_button(id: &'static str, label: &str, primary: bool) -> gpui::Stateful<Div> {
    button_base(id, primary).child(SharedString::from(label.to_string()))
}

fn hint_button(id: &'static str, label: &str, hint: Option<String>, primary: bool) -> gpui::Stateful<Div> {
    button_base(id, primary)
        .child(SharedString::from(label.to_string()))
        .when_some(hint, |d, hint| {
            d.child(
                div()
                    .text_color(if primary { rgba(0xffffffb3) } else { rgb(MUTED_COLOR) })
                    .child(hint),
            )
        })
}

fn sidebar_heading(label: &'static str) -> Div {
    div()
        .mb(px(8.0))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED_COLOR))
        .child(label)
}

fn split_timeline_line(line: &str) -> (&str, &str, bool) {
    let (time, rest) = line.split_once(' ').unwrap_or(("", line));
    if let Some(body) = rest.strip_prefix("✎ ") {
        (time, body, true)
    } else {
        (time, rest.strip_prefix("· ").unwrap_or(rest), false)
    }
}

fn timeline_row(index: usize, line: &str) -> gpui::Stateful<Div> {
    let (time, body, is_mark) = split_timeline_line(line);
    div()
        .id(("timeline-row", index))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .flex()
        .items_start()
        .gap(px(10.0))
        .text_size(px(12.5))
        .line_height(relative(1.4))
        .hover(|s| s.bg(rgb(SURFACE_BG)))
        .child(div().w(px(32.0)).flex_none().text_color(rgb(MUTED_COLOR)).child(time.to_string()))
        .when(is_mark, |d| d.child(div().flex_none().text_color(rgb(INK_RED)).child("✎")))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(if is_mark { rgb(TEXT_COLOR) } else { rgb(TEXT_SOFT) })
                .child(body.to_string()),
        )
}

fn open(cx: &mut App, dir: PathBuf, bundle: ScrawlBundle) {
    let bounds = Bounds::centered(None, size(px(1120.0), px(740.0)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Scrawl".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            focus: true,
            show: true,
            kind: WindowKind::Normal,
            is_resizable: true,
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some(APP_ID.into()),
            ..Default::default()
        },
        move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus);
            cx.new(|_| ReviewWindow::new(dir, bundle, focus))
        },
    );
}

/// Opens the review window for one bundle and runs until it is closed.
pub fn run(dir: PathBuf, bundle: ScrawlBundle) {
    Application::new().run(move |cx: &mut App| {
        open(cx, dir, bundle);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
