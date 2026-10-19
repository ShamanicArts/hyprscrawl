//! Linux stand-in for `ui.rs` (GPUI + AppKit menu-bar app). `run_app` is the background daemon
//! (`crate::linux::daemon`); the GPUI review window and picker come later.

use std::path::PathBuf;
use std::time::Instant;

use crate::model::ScrawlBundle;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionMode {
    Area,
    Window,
    /// The whole display the picker covers.
    Screen,
    Iphone,
}

pub fn run_app(
    initial_bundle: Option<(PathBuf, ScrawlBundle)>,
    start_recording_immediately: bool,
    start_iphone_immediately: bool,
    screenshot_out: Option<PathBuf>,
) {
    if start_iphone_immediately {
        eprintln!("iPhone capture is macOS-only");
    }
    if let Some((dir, _)) = initial_bundle {
        println!("{}", dir.display());
    }
    let _ = screenshot_out;
    if let Err(e) = crate::linux::daemon::run(start_recording_immediately) {
        eprintln!("scrawl: {e:#}");
        std::process::exit(1);
    }
}

pub fn run_start_benchmark() {
    eprintln!("SCRAWL_BENCH_START is macOS-only");
}

pub(crate) fn start_metric(name: &str, started: Instant) {
    crate::capture::scrawl_log(format!("{name} {} ms", started.elapsed().as_millis()));
}

/// The capsule motion lab needs the GPUI capsule (macOS for now).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum LabScene {
    TapThenArea,
    TapThenWindow,
    TapThenScreen,
    HoldSwitcher,
    Finish,
}

pub struct LabOptions {
    pub scene: LabScene,
    pub repeat: u32,
    pub backdrop: bool,
}

pub fn run_lab(options: LabOptions) {
    let _ = (options.scene, options.repeat, options.backdrop);
    eprintln!("scrawl lab is macOS-only for now");
}

pub(crate) enum TuneAction {
    List,
    Set(String, f32),
    Preset(usize),
    Reset,
    Rust,
}

pub(crate) fn tune_command(action: TuneAction) -> anyhow::Result<()> {
    let _ = matches!(action, TuneAction::Set(..) | TuneAction::Preset(_) | TuneAction::List | TuneAction::Reset | TuneAction::Rust);
    anyhow::bail!("scrawl tune drives the GPUI capsule, which is macOS-only for now")
}
