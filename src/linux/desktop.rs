//! Clipboard (`wl-copy`), paste (`wtype`), notifications (`omarchy-notification-send`, else
//! `notify-send`) and Omarchy's bar indicator refresh.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

fn has(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

fn wl_copy(mime: &str, bytes: &[u8]) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not run wl-copy (install wl-clipboard)")?;
    let written = child.stdin.take().context("wl-copy stdin").and_then(|mut stdin| Ok(stdin.write_all(bytes)?));
    // wl-copy forks to serve the selection; the parent exits once it has the data.
    let status = child.wait()?;
    written?;
    if !status.success() {
        bail!("wl-copy failed");
    }
    Ok(())
}

pub fn copy_text(text: &str) -> Result<()> {
    wl_copy("text/plain;charset=utf-8", text.as_bytes())
}

pub fn copy_png(png: &[u8]) -> Result<()> {
    wl_copy("image/png", png)
}

/// Waits (up to `max_ms`) until no modifier is held, so a synthetic paste isn't read as
/// Alt+Shift+V or similar.
pub fn wait_for_modifiers_released(max_ms: u64) {
    let lua = r#"local function d(k) local ok, r = pcall(hl.is_key_down, k); return ok and r end
return (d("Shift_L") or d("Shift_R") or d("Control_L") or d("Control_R") or d("Alt_L") or d("Alt_R") or d("Super_L") or d("Super_R")) and "held" or "free""#;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(max_ms);
    while std::time::Instant::now() < deadline {
        match crate::linux::hypr::eval(lua) {
            Ok(r) if r.contains("held") => std::thread::sleep(std::time::Duration::from_millis(15)),
            _ => return,
        }
    }
}

/// Ctrl+Shift+V in terminals, Ctrl+V elsewhere.
pub fn paste_into_focused() -> Result<()> {
    wait_for_modifiers_released(400);
    let args: &[&str] = if crate::capture::is_frontmost_terminal() {
        &["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"]
    } else {
        &["-M", "ctrl", "-k", "v", "-m", "ctrl"]
    };
    let status = Command::new("wtype").args(args).status().context("Could not run wtype")?;
    if !status.success() {
        bail!("wtype failed");
    }
    Ok(())
}

/// Puts the bundle's summary (transcript, keyframe index, absolute paths) on the clipboard,
/// the plain-text half of upstream's pasteboard handoff.
pub fn copy_bundle(bundle_dir: &Path) -> Result<()> {
    let (directory, bundle) = crate::handoff::load_bundle(bundle_dir)?;
    copy_text(&crate::sketch::summary_markdown(&bundle, &directory))
}

/// The contact sheet first (terminals get its path, which agents resolve), then the summary,
/// pasted in turn into the focused window.
pub fn paste_bundle(bundle_dir: &Path) -> Result<()> {
    let (directory, bundle) = crate::handoff::load_bundle(bundle_dir)?;
    let sheet = directory.join(&bundle.contact_sheet);
    let summary = crate::sketch::summary_markdown(&bundle, &directory);
    if crate::capture::is_frontmost_terminal() {
        copy_text(&sheet.display().to_string())?;
    } else {
        copy_png(&std::fs::read(&sheet).context("Could not read contact sheet")?)?;
    }
    std::thread::sleep(std::time::Duration::from_millis(40));
    paste_into_focused()?;
    std::thread::sleep(std::time::Duration::from_millis(250));
    copy_text(&summary)?;
    std::thread::sleep(std::time::Duration::from_millis(40));
    paste_into_focused()
}

pub struct Notice<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub image: Option<&'a Path>,
    pub on_click: Option<Vec<String>>,
    pub replaces: Option<u32>,
    pub critical: bool,
}

/// Shows a desktop notification; returns its id when the notifier reports one.
pub fn notify(n: Notice) -> Option<u32> {
    if std::env::var_os("SCRAWL_NO_NOTIFY").is_some() {
        crate::capture::scrawl_log(format!("notice: {} — {}", n.title, n.body));
        return None;
    }
    if has("omarchy-notification-send") {
        // Sent as Omarchy's default "omarchy-action", the app name for confirmations of something
        // the user just did ("Screenshot saved"): those are the toasts Do Not Disturb lets through,
        // and every Scrawl notice answers a key the user just pressed.
        let mut cmd = Command::new("omarchy-notification-send");
        cmd.arg("-p");
        if n.critical {
            cmd.args(["-u", "critical"]);
        }
        if let Some(id) = n.replaces {
            cmd.args(["-r", &id.to_string()]);
        }
        if let Some(img) = n.image {
            cmd.arg("--image").arg(img);
        }
        cmd.arg(n.title).arg(n.body);
        if let Some(exec) = &n.on_click {
            cmd.arg("--exec").args(exec);
        }
        let out = cmd.stderr(Stdio::null()).output().ok()?;
        return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
    }
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "Scrawl", "-p"]);
    if n.critical {
        cmd.args(["-u", "critical"]);
    }
    if let Some(id) = n.replaces {
        cmd.args(["-r", &id.to_string()]);
    }
    if let Some(img) = n.image {
        cmd.arg("-i").arg(img);
    }
    let out = cmd.arg(n.title).arg(n.body).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

pub fn notify_simple(title: &str, body: &str) -> Option<u32> {
    notify(Notice {
        title,
        body,
        image: None,
        on_click: None,
        replaces: None,
        critical: false,
    })
}

/// Omarchy's bar polls `pgrep ^gpu-screen-recorder`; ask it to look now.
pub fn refresh_indicators() {
    if has("omarchy-shell")
        && let Ok(mut child) = Command::new("omarchy-shell")
            .args(["-q", "omarchy.indicators", "refresh"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    {
        // Reaped off the caller's thread: the daemon lives long and would collect zombies.
        std::thread::spawn(move || child.wait());
    }
}
