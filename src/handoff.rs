use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::model::ScrawlBundle;

pub fn default_captures_dir() -> PathBuf {
    std::env::var_os("SCRAWL_CAPTURES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Library/Application Support/scrawl/captures")
        })
}

pub fn resolve_bundle_dir(path_or_id: &Path) -> Result<PathBuf> {
    if path_or_id == Path::new("latest") {
        return newest_bundle(&default_captures_dir());
    }
    let mut path = path_or_id.to_owned();
    if path.file_name().is_some_and(|name| name == "scrawl.json") {
        path = path
            .parent()
            .context("Missing bundle directory")?
            .to_owned();
        if path.as_os_str().is_empty() {
            path = PathBuf::from(".");
        }
    }
    if !path.exists() && path.components().count() == 1 {
        path = default_captures_dir().join(&path);
        if path.extension().is_none() {
            path.set_extension("scrawl");
        }
    }
    ensure!(
        path.is_dir(),
        "Bundle directory not found: {}",
        path.display()
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("Could not resolve {}", path.display()))?;
    ensure!(
        path.extension().is_some_and(|ext| ext == "scrawl"),
        "Expected a .scrawl directory: {}",
        path.display()
    );
    Ok(path)
}

fn newest_bundle(captures: &Path) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    for entry in
        fs::read_dir(captures).with_context(|| format!("Could not read {}", captures.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "scrawl") && path.is_dir() {
            // Only finished bundles count: a capture still recording, or one cut off by a crash,
            // has no valid manifest yet and would otherwise hide the last good one (its media is
            // left in place). Bundle creation time is stable even after later zoom-frame extraction.
            if let Some(created) = fs::read(path.join("scrawl.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<ScrawlBundle>(&bytes).ok())
                .map(|bundle| bundle.created_at_ms)
            {
                candidates.push((created, path));
            }
        }
    }
    candidates.sort_unstable();
    candidates
        .pop()
        .with_context(|| format!("No finished .scrawl captures in {}", captures.display()))?
        .1
        .canonicalize()
        .map_err(Into::into)
}

pub fn load_bundle(bundle_dir: &Path) -> Result<(PathBuf, ScrawlBundle)> {
    let directory = resolve_bundle_dir(bundle_dir)?;
    let path = directory.join("scrawl.json");
    let bundle = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Could not read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid bundle manifest: {}", path.display()))?;
    Ok((directory, bundle))
}

#[cfg(target_os = "macos")]
pub fn copy_string_to_clipboard(content: &str) -> Result<()> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSString};

    let text = NSString::from_str(content);
    let pasteboard = NSPasteboard::generalPasteboard();
    unsafe {
        let types = NSArray::from_slice(&[NSPasteboardTypeString]);
        pasteboard.clearContents();
        pasteboard.declareTypes_owner(&types, None);
        ensure!(
            pasteboard.setString_forType(&text, NSPasteboardTypeString),
            "Could not copy text to clipboard"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn copy_png_to_clipboard(png_bytes: &[u8]) -> Result<()> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
    use objc2_foundation::{NSArray, NSData};

    let data = NSData::with_bytes(png_bytes);
    let pasteboard = NSPasteboard::generalPasteboard();
    unsafe {
        let types = NSArray::from_slice(&[NSPasteboardTypePNG]);
        pasteboard.clearContents();
        pasteboard.declareTypes_owner(&types, None);
        ensure!(
            pasteboard.setData_forType(Some(&data), NSPasteboardTypePNG),
            "Could not copy PNG image to clipboard"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn copy_bundle_to_clipboard(bundle_dir: &Path) -> Result<()> {
    let (directory, bundle) = load_bundle(bundle_dir)?;
    let summary = crate::sketch::summary_markdown(&bundle, &directory);
    copy_string_to_clipboard(&summary)
}

/// Pastes both the contact-sheet image and the transcript/keyframe text into the active app
/// as a two-step sequence (`⌘V` image, then `⌘V` text). If `click_target` is provided (from
/// dragging the Paste chip onto a window/input), clicks at that screen coordinate first to focus it.
#[cfg(target_os = "macos")]
pub fn paste_bundle_at_target(bundle_dir: &Path, click_target: Option<[f32; 2]>) -> Result<()> {
    if let Some([x, y]) = click_target {
        crate::capture::post_synthetic_click(f64::from(x), f64::from(y));
        std::thread::sleep(std::time::Duration::from_millis(65));
    } else {
        crate::capture::ensure_external_app_active();
    }
    paste_bundle_image_and_text(bundle_dir)
}

#[cfg(target_os = "macos")]
pub fn paste_bundle_image_and_text(bundle_dir: &Path) -> Result<()> {
    let (directory, bundle) = load_bundle(bundle_dir)?;
    let sheet_path = directory.join(&bundle.contact_sheet);
    let image = fs::read(&sheet_path).context("Could not read contact sheet")?;
    let summary = crate::sketch::summary_markdown(&bundle, &directory);

    crate::capture::wait_for_modifiers_released(350);

    if crate::capture::is_frontmost_terminal() {
        // In terminals (Ghostty / OpenCode TUI / Claude Code), pasting the image path first
        // resolves and attaches `[Image 1]`, and the second paste inserts the text summary.
        copy_string_to_clipboard(&sheet_path.display().to_string())?;
    } else {
        // In GUI apps (OpenCode Desktop, Slack, Linear, browsers), PNG-only attaches the image.
        copy_png_to_clipboard(&image)?;
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::capture::post_synthetic_cmd_v();

    std::thread::sleep(std::time::Duration::from_millis(160));
    copy_string_to_clipboard(&summary)?;
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::capture::post_synthetic_cmd_v();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn copy_bundle_to_clipboard(_bundle_dir: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    return crate::linux::desktop::copy_bundle(_bundle_dir);
    #[cfg(not(target_os = "linux"))]
    bail!("Image and text clipboard handoff requires macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn paste_bundle_image_and_text(_bundle_dir: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    return crate::linux::desktop::paste_bundle(_bundle_dir);
    #[cfg(not(target_os = "linux"))]
    bail!("Image and text paste handoff requires macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn paste_bundle_at_target(_bundle_dir: &Path, _click_target: Option<[f32; 2]>) -> Result<()> {
    bail!("Image and text paste handoff requires macOS")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpenCodeSession {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "parentID")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub time: OpenCodeSessionTime,
    /// V2 puts the session's working directory here; V1 used a top-level `directory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<OpenCodeSessionLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
}

impl OpenCodeSession {
    pub fn directory(&self) -> Option<&str> {
        self.location
            .as_ref()
            .and_then(|l| l.directory.as_deref())
            .or(self.directory.as_deref())
            .filter(|d| !d.is_empty())
    }

    pub fn is_open_root(&self) -> bool {
        self.parent_id.is_none() && self.time.archived.is_none()
    }

    /// The title, or the ID when it has none.
    pub fn label(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OpenCodeSessionLocation {
    #[serde(default)]
    pub directory: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OpenCodeSessionTime {
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub archived: Option<u64>,
}

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(name))
            .find(|path| executable(path))
    })
}

fn opencode_binary() -> Result<PathBuf> {
    if let Some(bin) = std::env::var_os("OPENCODE_BIN").filter(|bin| !bin.is_empty()) {
        let path = PathBuf::from(bin);
        if executable(&path) {
            return Ok(path);
        }
        if let Some(path) = path.to_str().and_then(on_path) {
            return Ok(path);
        }
        bail!("OPENCODE_BIN is not an executable");
    }
    if let Some(path) = on_path("opencode2") {
        return Ok(path);
    }
    if let Some(path) = dirs::home_dir()
        .map(|home| home.join("bin/opencode2"))
        .filter(|path| executable(path))
    {
        return Ok(path);
    }
    bail!("Could not find opencode2; set OPENCODE_BIN or add it to PATH")
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub agent_host: Option<String>,
    #[serde(default)]
    pub agent_opencode: Option<String>,
    #[serde(default)]
    pub ingest_token: Option<String>,
    #[serde(default, alias = "tetherApps")]
    pub usb_apps: Vec<String>,
    /// `false` turns Screen-capture auto-crop off; an object overrides its settings.
    #[serde(default)]
    pub autocrop: Option<serde_json::Value>,
    /// `false` keeps Mac captures of a dev site on the clipboard instead of sending them to the
    /// site's OpenCode session (`routing`).
    #[serde(default)]
    pub auto_send: Option<bool>,
    /// Feature flag: shows **Export Last Capture as Video…** in the menu (default off).
    #[serde(default)]
    pub video_export: Option<bool>,
    /// `"apple"` (default) or `"hex"`: which engine transcribes recordings (`transcribe`).
    #[serde(default)]
    pub transcriber: Option<String>,
    /// The Hex model to use when `transcriber` is `"hex"` (default: the first ready one of
    /// `hex::PREFERRED_MODELS`).
    #[serde(default)]
    pub hex_model: Option<String>,
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("SCRAWL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config/scrawl/config.json")
        })
}

pub fn load_config_from(path: &Path) -> Config {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn config() -> Config {
    load_config_from(&default_config_path())
}

fn random_hex_token_32() -> Result<String> {
    use std::fmt::Write as _;
    use std::io::Read as _;
    let mut file = fs::File::open("/dev/urandom").context("Could not open /dev/urandom")?;
    let mut bytes = [0_u8; 32];
    file.read_exact(&mut bytes)
        .context("Could not read 32 bytes from /dev/urandom")?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(&mut token, "{byte:02x}");
    }
    Ok(token)
}

/// Returns `ingestToken` from `path`, generating a 32-byte hex token and writing `path` back with
/// mode `0600` (while preserving all other JSON keys) if `ingestToken` is absent or empty.
pub fn ensure_ingest_token_in(path: &Path) -> Result<String> {
    let mut map = match fs::read(path) {
        Ok(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => {
            match serde_json::from_slice::<serde_json::Value>(&bytes)
                .with_context(|| format!("Invalid JSON in {}", path.display()))?
            {
                serde_json::Value::Object(map) => map,
                _ => bail!("Expected a JSON object in {}", path.display()),
            }
        }
        Ok(_) | Err(_) => serde_json::Map::new(),
    };

    if let Some(existing) = map
        .get("ingestToken")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(existing.to_owned());
    }

    let token = random_hex_token_32()?;
    map.insert(
        "ingestToken".to_owned(),
        serde_json::Value::String(token.clone()),
    );
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
    }
    let mut body = serde_json::to_vec_pretty(&serde_json::Value::Object(map))?;
    body.push(b'\n');
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("Could not write {}", path.display()))?;
        file.write_all(&body)?;
        file.sync_all()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, &body).with_context(|| format!("Could not write {}", path.display()))?;
    }
    Ok(token)
}

pub fn ensure_ingest_token() -> Result<String> {
    ensure_ingest_token_in(&default_config_path())
}

/// Whether Mac captures of a dev site go straight to its session (`autoSend`, default on).
pub fn auto_send() -> bool {
    config().auto_send.unwrap_or(true)
}

/// Whether the menu offers video export (`videoExport`, default off). `scrawl export` works
/// either way.
pub fn video_export() -> bool {
    config().video_export.unwrap_or(false)
}

/// The configured transcriber name (`transcriber`) as written, unvalidated.
pub fn transcriber() -> Option<String> {
    config().transcriber
}

/// The configured Hex model id (`hexModel`), if any.
pub fn hex_model() -> Option<String> {
    config()
        .hex_model
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
}

/// Sets one top-level key in `path`, keeping the rest (and its `0600` mode, since it holds the
/// ingest token).
pub fn set_config_value_in(path: &Path, key: &str, value: serde_json::Value) -> Result<()> {
    let mut config = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    config[key] = value;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut body = serde_json::to_vec_pretty(&config)?;
    body.push(b'\n');
    fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("Could not write {}", path.display()))
}

/// Bundle IDs whose `Documents/Scrawl` (and legacy `Documents/Tether`) container the USB watcher
/// checks (`usbApps` in config, with `tetherApps` accepted as a fallback alias; default empty so
/// USB polling is off unless configured).
pub fn usb_apps() -> Vec<String> {
    config()
        .usb_apps
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| safe_word(s))
        .collect()
}

/// Safe to put unquoted in a remote shell command (host names, `~/bin/tool` paths).
fn safe_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@/~".contains(&b))
}

/// The Mac whose OpenCode server holds Kit's sessions when it isn't this one: an SSH host from
/// `SCRAWL_AGENT_HOST` or `~/.config/scrawl/config.json` (`{"agentHost": "studio"}`).
pub fn agent_host() -> Option<String> {
    std::env::var("SCRAWL_AGENT_HOST")
        .ok()
        .or_else(|| config().agent_host)
        .filter(|h| safe_word(h))
}

/// The `opencode2` command to run on the agent host (`agentOpencode`, default `opencode2`).
fn agent_opencode() -> String {
    config()
        .agent_opencode
        .filter(|c| safe_word(c))
        .unwrap_or_else(|| "opencode2".to_owned())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn ssh(host: &str) -> Command {
    let mut command = Command::new("/usr/bin/ssh");
    command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", host]);
    command
}

/// Bundle files too big to hold up queueing the session; they follow in the background.
pub const HEAVY_MEDIA: [&str; 5] = [
    "recording.mp4",
    "recording.mov",
    "audio.wav",
    "frames-full",
    "contact-sheet-full.png",
];
/// Present in a copied bundle while its heavy media is still arriving.
pub const MEDIA_PENDING: &str = ".media-pending";

/// Copies a bundle to the same path on `host`, so the agent there can open its files.
/// Syncs the contact sheet, keyframe PNGs, manifest, and summary synchronously so the session
/// can be queued immediately, then streams heavy media (`recording.mp4`/`.mov`, `frames-full`)
/// in the background for `scrawl zoom`, marked by [`MEDIA_PENDING`] until it lands.
pub fn sync_bundle_to(host: &str, bundle_dir: &Path) -> Result<()> {
    use std::process::Stdio;
    let parent = bundle_dir.parent().context("Bundle has no parent")?;
    let name = bundle_dir.file_name().context("Bundle has no name")?;
    let mut tar = Command::new("/usr/bin/tar")
        .arg("-C")
        .arg(parent)
        .args([
            "--exclude=*/recording.mp4",
            "--exclude=*/recording.mov",
            "--exclude=*/audio.wav",
            "--exclude=*/frames-full",
            "--exclude=*/contact-sheet-full.png",
            "--exclude=*/.media-sending",
        ])
        .arg("-cf")
        .arg("-")
        .arg(name)
        .stdout(Stdio::piped())
        .spawn()
        .context("Could not start tar")?;
    let parent_q = shell_quote(&parent.to_string_lossy());
    let name_str = name.to_string_lossy().into_owned();
    let heavy: Vec<String> = HEAVY_MEDIA
        .iter()
        .filter(|rel| bundle_dir.join(rel).exists())
        .map(|rel| format!("{name_str}/{rel}"))
        .collect();
    let bundle_q = shell_quote(&parent.join(&name_str).to_string_lossy());
    // The marker goes up with the light files, before the session is prompted, so `scrawl zoom`
    // there knows the video is still on its way rather than missing.
    let mark_pending = if heavy.is_empty() {
        String::new()
    } else {
        format!(" && touch {bundle_q}/{MEDIA_PENDING}")
    };
    let output = ssh(host)
        .arg(format!("mkdir -p {parent_q} && tar -C {parent_q} -xf -{mark_pending}"))
        .stdin(tar.stdout.take().context("No tar output")?)
        .output()
        .context("Could not start ssh")?;
    let _ = tar.wait();
    ensure!(
        output.status.success(),
        "Copying the bundle to {host} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    if !heavy.is_empty() {
        let host_owned = host.to_owned();
        let parent_owned = parent.to_path_buf();
        // Unpack into a staging folder and move into the bundle only once complete, so nothing on
        // the other side ever reads a half-copied video; then clear the marker.
        let staging_q = shell_quote(
            &parent
                .join(format!(".{name_str}.incoming"))
                .to_string_lossy(),
        );
        let remote = format!(
            "rm -rf {staging_q} && mkdir -p {staging_q} && tar -C {staging_q} -xf - && \
             for f in {staging_q}/*/*; do rm -rf {bundle_q}/\"$(basename \"$f\")\" && mv \"$f\" {bundle_q}/; done; \
             rm -rf {staging_q} {bundle_q}/{MEDIA_PENDING}"
        );
        // Held until the media lands, so the iPhone video isn't re-encoded or cleaned up under
        // the copy (see `retention`).
        let sending = bundle_dir.join(crate::retention::MEDIA_SENDING);
        let _ = fs::write(&sending, host);
        std::thread::spawn(move || {
            let _done = RemoveOnDrop(sending);
            let Ok(mut bg_tar) = Command::new("/usr/bin/tar")
                .arg("-C")
                .arg(&parent_owned)
                .arg("-cf")
                .arg("-")
                .args(&heavy)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };
            if let Some(stdout) = bg_tar.stdout.take() {
                let _ = ssh(&host_owned)
                    .arg(remote)
                    .stdin(stdout)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = bg_tar.wait();
        });
    }
    Ok(())
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn opencode_api(method: &str, path: &str, data: Option<&serde_json::Value>) -> Result<Vec<u8>> {
    let mut command = if let Some(host) = agent_host() {
        let mut remote = format!(
            "PATH=\"$HOME/bin:$HOME/.opencode/bin:/opt/homebrew/bin:$PATH\" {} api {} {}",
            agent_opencode(),
            shell_quote(method),
            shell_quote(path)
        );
        if let Some(data) = data {
            remote.push_str(&format!(
                " --data {}",
                shell_quote(&serde_json::to_string(data)?)
            ));
        }
        let mut command = ssh(&host);
        command.arg(remote);
        command
    } else {
        let mut command = Command::new(opencode_binary()?);
        command.args(["api", method, path]);
        if let Some(data) = data {
            command.args(["--data", &serde_json::to_string(data)?]);
        }
        command
    };
    let output = command.output().context("Could not launch opencode2")?;
    ensure!(
        output.status.success(),
        "OpenCode API failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

pub(crate) fn query_escape(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

pub fn list_opencode_sessions() -> Result<Vec<OpenCodeSession>> {
    list_sessions_from("/api/session")
}

/// Root sessions only (the server filters them), newest first.
pub fn list_root_sessions() -> Result<Vec<OpenCodeSession>> {
    list_sessions_from("/api/session?parentID=null&limit=100")
}

fn list_sessions_from(first: &str) -> Result<Vec<OpenCodeSession>> {
    let mut sessions = Vec::new();
    let mut path = first.to_owned();
    let mut cursors = std::collections::HashSet::new();
    loop {
        let response: serde_json::Value =
            serde_json::from_slice(&opencode_api("get", &path, None)?)
                .context("Invalid OpenCode session response")?;
        let data = response
            .as_array()
            .or_else(|| response.get("data").and_then(|data| data.as_array()))
            .context("Missing OpenCode session list")?;
        for session in data {
            sessions.push(serde_json::from_value::<OpenCodeSession>(session.clone())?);
        }
        let Some(next) = response.pointer("/cursor/next").and_then(|v| v.as_str()) else {
            break;
        };
        ensure!(
            cursors.insert(next.to_owned()),
            "OpenCode returned a repeated session cursor"
        );
        path = format!("/api/session?cursor={}", query_escape(next));
    }
    sessions.sort_by(|a, b| {
        b.time
            .updated
            .cmp(&a.time.updated)
            .then_with(|| a.id.cmp(&b.id))
    });
    sessions.dedup_by(|a, b| a.id == b.id);
    Ok(sessions)
}

fn normalize_slug(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Sends a bundle to `session_id`, else to the session its recording was addressed to, else the
/// session the app resolves to. Sending an addressed recording elsewhere needs `reroute`.
pub fn send_to_opencode(
    bundle_dir: &Path,
    session_id: Option<&str>,
    reroute: bool,
) -> Result<String> {
    let addressed = recorded_destination(&resolve_bundle_dir(bundle_dir)?);
    let session_id = choose_destination(session_id, addressed.as_deref(), reroute)?;
    let (id, _) = send_to_opencode_for_app(bundle_dir, session_id, None)?;
    Ok(id)
}

/// The session a recording was addressed to: a ScrawlKit `timeline.json`'s top-level `session`,
/// else the `destination` a Mac capture of a dev site was routed to (`scrawl.json`).
/// `context.session` only names the session that was on screen, and never chooses a destination.
pub fn recorded_destination(bundle_dir: &Path) -> Option<String> {
    let read = |name: &str| -> Option<serde_json::Value> {
        serde_json::from_slice(&fs::read(bundle_dir.join(name)).ok()?).ok()
    };
    let session = |value: Option<&serde_json::Value>| {
        value
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    read("timeline.json")
        .and_then(|timeline| session(timeline.get("session")))
        .or_else(|| read("scrawl.json").and_then(|b| session(b.pointer("/destination/session"))))
}

fn choose_destination<'a>(
    requested: Option<&'a str>,
    addressed: Option<&'a str>,
    reroute: bool,
) -> Result<Option<&'a str>> {
    match (requested, addressed) {
        (Some(requested), Some(addressed)) if requested != addressed && !reroute => bail!(
            "This recording was addressed to {addressed}; pass --reroute to send it to {requested} instead"
        ),
        (Some(requested), _) => Ok(Some(requested)),
        (None, addressed) => Ok(addressed),
    }
}

pub fn send_to_opencode_for_app(
    bundle_dir: &Path,
    session_id: Option<&str>,
    app_name: Option<&str>,
) -> Result<(String, String)> {
    let (_, bundle) = load_bundle(bundle_dir)?;
    let (session, title) =
        resolve_opencode_session(session_id, app_name.or(bundle.target.app.as_deref()))?;
    send_bundle_to_session(bundle_dir, &session)?;
    Ok((session, title))
}

/// Picks the session a capture goes to, and its title: an explicit ID, else the root session
/// whose title matches the app ("UpDown" → "UP DOWN APP"), else `OPENCODE_SESSION_ID`, else the
/// most recently updated root session.
pub fn resolve_opencode_session(
    session_id: Option<&str>,
    app_name: Option<&str>,
) -> Result<(String, String)> {
    let app_slug = app_name.map(normalize_slug).filter(|s| !s.is_empty());

    let (session, session_title) = if let Some(id) = session_id.filter(|s| !s.trim().is_empty()) {
        let title = list_opencode_sessions()
            .ok()
            .and_then(|list| list.into_iter().find(|s| s.id == id).map(|s| s.title))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| id.to_owned());
        (id.to_owned(), title)
    } else {
        let roots: Vec<OpenCodeSession> = list_opencode_sessions()?
            .into_iter()
            .filter(|session| session.parent_id.is_none() && session.time.archived.is_none())
            .collect();
        let matched = app_slug
            .as_deref()
            .and_then(|slug| {
                // Match against the last component if bundle identifier was passed (e.g. com.kitlangton.UpDown -> updown).
                let short = app_name
                    .and_then(|a| a.rsplit('.').next())
                    .map(normalize_slug)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| slug.to_owned());
                roots.iter().find(|s| {
                    let title_slug = normalize_slug(&s.title);
                    !title_slug.is_empty()
                        && (title_slug.contains(&short) || short.contains(&title_slug))
                })
            })
            .cloned()
            .or_else(|| {
                if let Some(id) = std::env::var("OPENCODE_SESSION_ID")
                    .ok()
                    .filter(|id| !id.trim().is_empty())
                {
                    roots.iter().find(|s| s.id == id).cloned()
                } else {
                    None
                }
            })
            .or_else(|| roots.into_iter().next())
            .context("No root OpenCode session found; pass a session ID")?;
        let title = if matched.title.is_empty() {
            matched.id.clone()
        } else {
            matched.title
        };
        (matched.id, title)
    };
    Ok((session, session_title))
}

/// Queues a bundle's summary and pointers into an OpenCode session.
pub fn send_bundle_to_session(bundle_dir: &Path, session: &str) -> Result<()> {
    let (directory, bundle) = load_bundle(bundle_dir)?;
    let summary = crate::sketch::summary_markdown(&bundle, &directory);
    ensure!(
        session.starts_with("ses")
            && session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "Invalid OpenCode session ID"
    );
    if let Some(host) = agent_host() {
        sync_bundle_to(&host, &directory)?;
    }
    let quoted_directory = shell_quote(&directory.to_string_lossy());
    let contact_sheet = directory.join(&bundle.contact_sheet);
    let text = format!(
        "{}\n\nThe contact sheet is attached ({}); read {} for timestamped transcript, marks, and interactions.\nTo look closely at a moment (an animation, a glitch, what the narration points at), run:\nscrawl zoom {} --at <M:SS or \"a phrase that was said\"> [--span 2] [--frames 12]\nIt saves a dense contact sheet of that moment with each frame's time and prints what was said; open the PNG before acting on the feedback. This capture is {} ms long.",
        summary.trim_end(),
        contact_sheet.display(),
        directory.join("scrawl.json").display(),
        quoted_directory,
        bundle.duration_ms,
    );
    // The server reads `file://` attachments from its own disk; the bundle is at the same path
    // there (copied above when the agents run on another Mac).
    opencode_api(
        "post",
        &format!("/api/session/{session}/prompt"),
        Some(&serde_json::json!({
            "text": text,
            "files": [{
                "uri": file_uri(&contact_sheet),
                "name": "contact-sheet.png",
                "description": "Scrawl contact sheet: the capture's keyframes in order",
            }],
            "delivery": "queue",
        })),
    )?;
    Ok(())
}

/// A `file://` URL for an absolute path, percent-encoding everything but unreserved
/// characters and `/` ("Application Support" → "Application%20Support").
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

pub fn extract_frames_range(
    bundle_dir: &Path,
    from_ms: u64,
    to_ms: u64,
    fps: u32,
) -> Result<Vec<PathBuf>> {
    ensure!(from_ms <= to_ms, "Frame range ends before it starts");
    ensure!(fps > 0 && fps <= 240, "FPS must be between 1 and 240");
    let directory = resolve_bundle_dir(bundle_dir)?;
    let loaded = load_bundle(&directory).ok().map(|(_, bundle)| bundle);
    let offset_ms = loaded.as_ref().map_or(0, |b| b.video_offset_ms);
    let video = loaded
        .and_then(|bundle| bundle.video)
        .map_or_else(|| directory.join("recording.mp4"), |v| directory.join(v));
    if video.is_file() && from_ms < to_ms {
        let output_dir = directory
            .join("zoom")
            .join(format!("range-{from_ms}-{to_ms}"));
        if let Some(ffmpeg) = on_path("ffmpeg") {
            fs::create_dir_all(&output_dir)?;
            // Only remove our generated files, so repeating at a lower FPS cannot
            // return leftover frames from a previous extraction.
            for entry in fs::read_dir(&output_dir)? {
                let entry = entry?;
                if generated_frame(&entry.path()) {
                    fs::remove_file(entry.path())?;
                }
            }
            let output = Command::new(ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-y",
                    "-ss",
                ])
                .arg(format!("{:.3}", from_ms as f64 / 1_000.0))
                .args(["-threads", "1", "-filter_threads", "1"])
                .arg("-i")
                .arg(&video)
                .arg("-t")
                .arg(format!("{:.3}", (to_ms - from_ms) as f64 / 1_000.0))
                .arg("-vf")
                .arg(format!("fps={fps}"))
                .args(["-threads", "1"])
                .arg(output_dir.join("frame-%03d.png"))
                .output()
                .context("Could not start ffmpeg")?;
            ensure!(
                output.status.success(),
                "ffmpeg extraction failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let mut frames = Vec::new();
            for entry in fs::read_dir(&output_dir)? {
                let path = entry?.path();
                if generated_frame(&path) {
                    frames.push(path);
                }
            }
            frames.sort();
            return Ok(frames);
        }

        let step_ms = (1_000 / u64::from(fps)).max(1);
        let times: Vec<u64> = (from_ms..=to_ms).step_by(step_ms as usize).collect();
        if !times.is_empty()
            && let Ok(decoded) = crate::movie_frames_at(&video, &times, offset_ms, 0)
            && !decoded.is_empty()
        {
            fs::create_dir_all(&output_dir)?;
            for entry in fs::read_dir(&output_dir)? {
                let entry = entry?;
                if generated_frame(&entry.path()) {
                    fs::remove_file(entry.path())?;
                }
            }
            let mut written = Vec::with_capacity(decoded.len());
            for (idx, frame) in decoded.iter().enumerate() {
                let out_path = output_dir.join(format!("frame-{:03}.png", idx + 1));
                crate::render::save_png(frame, &out_path)?;
                written.push(out_path);
            }
            return Ok(written);
        }
    }
    existing_keyframes(&directory, from_ms, to_ms, fps)
}

fn generated_frame(path: &Path) -> bool {
    path.is_file()
        && path.extension().is_some_and(|ext| ext == "png")
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("frame-"))
            .is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
}

fn existing_keyframes(
    directory: &Path,
    from_ms: u64,
    to_ms: u64,
    fps: u32,
) -> Result<Vec<PathBuf>> {
    let (_, bundle) = load_bundle(directory)?;
    let margin = 1_000_u64.div_ceil(u64::from(fps)) / 2;
    let mut frames = Vec::new();
    for frame in bundle.keyframes {
        if frame.t < from_ms.saturating_sub(margin) || frame.t > to_ms.saturating_add(margin) {
            continue;
        }
        let relative = Path::new(&frame.file);
        ensure!(
            !relative.is_absolute()
                && relative.components().all(|part| matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )),
            "Invalid keyframe path"
        );
        let path = directory.join(relative);
        if generated_frame(&path) {
            let canonical = path.canonicalize()?;
            ensure!(
                canonical.starts_with(directory.join("frames")),
                "Keyframe is outside bundle frames directory"
            );
            frames.push((frame.t, canonical));
        }
    }
    frames.sort_unstable();
    frames.dedup_by(|a, b| a.1 == b.1);
    Ok(frames.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "scrawl-handoff-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn bundle(&self, name: &str, created: u64) -> PathBuf {
            let path = self.0.join(format!("{name}.scrawl"));
            fs::create_dir_all(path.join("frames")).unwrap();
            let keyframes: Vec<_> = [0, 1_900, 2_500, 4_000, 5_100, 6_000].into_iter().enumerate().map(|(index, t)| {
                let file = format!("frames/frame-{index:03}.png");
                fs::write(path.join(&file), b"fixture").unwrap();
                serde_json::json!({"index": index, "t": t, "reason": "initial", "file": file, "event": ""})
            }).collect();
            let bundle = serde_json::json!({
                "version": 1, "id": name, "createdAtMs": created, "durationMs": 7_000,
                "target": {"mode": "display", "displayId": 1,
                    "frame": {"x": 0.0, "y": 0.0, "width": 100.0, "height": 100.0, "scale": 1.0}},
                "transcript": {"source": "none", "text": "", "segments": []},
                "strokes": [], "marks": [], "events": [], "keyframes": keyframes,
                "contactSheet": "contact-sheet.png"
            });
            fs::write(
                path.join("scrawl.json"),
                serde_json::to_vec(&bundle).unwrap(),
            )
            .unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_directory_manifest_and_newest_capture() {
        let fixture = Fixture::new();
        let older = fixture.bundle("old", 100);
        let newer = fixture.bundle("new", 200);
        assert_eq!(
            resolve_bundle_dir(&older).unwrap(),
            older.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_bundle_dir(&older.join("scrawl.json")).unwrap(),
            older.canonicalize().unwrap()
        );
        assert_eq!(
            newest_bundle(&fixture.0).unwrap(),
            newer.canonicalize().unwrap()
        );
        assert!(resolve_bundle_dir(&fixture.0).is_err());
        assert!(resolve_bundle_dir(&fixture.0.join("missing.scrawl")).is_err());
    }

    #[test]
    fn newest_capture_skips_unfinished_bundles() {
        let fixture = Fixture::new();
        let finished = fixture.bundle("finished", 100);
        // Newer, but cut off before its manifest was written (a crash, or still recording).
        let unfinished = fixture.0.join("unfinished.scrawl");
        fs::create_dir_all(&unfinished).unwrap();
        fs::write(unfinished.join("recording.mp4"), b"partial").unwrap();
        let broken = fixture.0.join("broken.scrawl");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("scrawl.json"), b"{not json").unwrap();
        assert_eq!(
            newest_bundle(&fixture.0).unwrap(),
            finished.canonicalize().unwrap()
        );
        fs::remove_dir_all(&finished).unwrap();
        let error = newest_bundle(&fixture.0).unwrap_err().to_string();
        assert!(error.contains("No finished .scrawl captures"), "{error}");
    }

    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(
            file_uri(Path::new(
                "/Users/kit/Library/Application Support/scrawl/captures/a b#1.scrawl/contact-sheet.png"
            )),
            "file:///Users/kit/Library/Application%20Support/scrawl/captures/a%20b%231.scrawl/contact-sheet.png"
        );
    }

    #[test]
    fn loads_manifest_and_reports_invalid_json() {
        let fixture = Fixture::new();
        let path = fixture.bundle("example", 123);
        let (resolved, bundle) = load_bundle(&path.join("scrawl.json")).unwrap();
        assert_eq!(resolved, path.canonicalize().unwrap());
        assert_eq!(bundle.id, "example");
        assert_eq!(bundle.created_at_ms, 123);
        fs::write(path.join("scrawl.json"), b"not json").unwrap();
        assert!(load_bundle(&path).is_err());
    }

    #[test]
    fn frame_fallback_uses_manifest_times_not_file_indices() {
        let fixture = Fixture::new();
        let path = fixture.bundle("example", 123);
        let frames = extract_frames_range(&path, 2_000, 5_000, 4).unwrap();
        let names: Vec<_> = frames
            .iter()
            .map(|frame| frame.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "frame-001.png",
                "frame-002.png",
                "frame-003.png",
                "frame-004.png"
            ]
        );
        assert!(
            extract_frames_range(&path, 10_000, 11_000, 4)
                .unwrap()
                .is_empty()
        );
        assert!(extract_frames_range(&path, 5_000, 2_000, 4).is_err());
        assert!(extract_frames_range(&path, 0, 2_000, 0).is_err());
    }

    #[test]
    fn recordings_go_to_their_addressed_session_not_the_viewed_one() {
        let dir = std::env::temp_dir().join(format!("scrawl-dest-{}", crate::make_bundle_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("timeline.json"),
            r#"{"session":"ses_ios","context":{"session":"ses_motion"}}"#,
        )
        .unwrap();
        let addressed = recorded_destination(&dir);
        assert_eq!(addressed.as_deref(), Some("ses_ios"));
        let addressed = addressed.as_deref();
        assert_eq!(choose_destination(None, addressed, false).unwrap(), Some("ses_ios"));
        assert!(choose_destination(Some("ses_motion"), addressed, false).is_err());
        assert_eq!(
            choose_destination(Some("ses_motion"), addressed, true).unwrap(),
            Some("ses_motion")
        );
        assert_eq!(choose_destination(Some("ses_ios"), addressed, false).unwrap(), Some("ses_ios"));
        assert_eq!(choose_destination(Some("ses_x"), None, false).unwrap(), Some("ses_x"));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(recorded_destination(&dir), None);
    }

    #[test]
    fn token_generation_persists_with_0600_and_preserves_other_keys() {
        let fixture = Fixture::new();
        let config_path = fixture.0.join("nested/config.json");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            br#"{"agentHost":"studio","agentOpencode":"~/bin/opencode2","usbApps":["com.kitlangton.UpDown"],"customKey":42}"#,
        )
        .unwrap();

        let generated = ensure_ingest_token_in(&config_path).unwrap();
        assert_eq!(generated.len(), 64);
        assert!(generated.bytes().all(|b| b.is_ascii_hexdigit()));

        // Calling a second time returns the same token without regenerating.
        let second = ensure_ingest_token_in(&config_path).unwrap();
        assert_eq!(generated, second);

        // Other keys are preserved and parsed properly.
        let parsed = load_config_from(&config_path);
        assert_eq!(parsed.agent_host.as_deref(), Some("studio"));
        assert_eq!(parsed.agent_opencode.as_deref(), Some("~/bin/opencode2"));
        assert_eq!(parsed.ingest_token.as_deref(), Some(generated.as_str()));
        assert_eq!(parsed.usb_apps, vec!["com.kitlangton.UpDown"]);

        let legacy_path = fixture.0.join("nested/legacy.json");
        fs::write(&legacy_path, br#"{"tetherApps":["com.kitlangton.UpDown"]}"#).unwrap();
        assert_eq!(
            load_config_from(&legacy_path).usb_apps,
            vec!["com.kitlangton.UpDown"]
        );

        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(raw["customKey"], serde_json::json!(42));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&config_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn transcriber_preference_round_trips_and_keeps_other_keys() {
        let fixture = Fixture::new();
        let path = fixture.0.join("config.json");
        fs::write(&path, br#"{"ingestToken":"abc","autoSend":false}"#).unwrap();
        let config = load_config_from(&path);
        assert_eq!((config.transcriber, config.hex_model), (None, None));
        set_config_value_in(&path, "transcriber", "hex".into()).unwrap();
        set_config_value_in(&path, "hexModel", "parakeet_v3".into()).unwrap();
        let config = load_config_from(&path);
        assert_eq!(config.transcriber.as_deref(), Some("hex"));
        assert_eq!(config.hex_model.as_deref(), Some("parakeet_v3"));
        assert_eq!(config.ingest_token.as_deref(), Some("abc"));
        assert_eq!(config.auto_send, Some(false));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn auto_send_defaults_on_and_the_toggle_keeps_other_keys() {
        let fixture = Fixture::new();
        let path = fixture.0.join("config.json");
        fs::write(&path, br#"{"ingestToken":"abc","retention":{"mediaDays":7}}"#).unwrap();
        assert_eq!(load_config_from(&path).auto_send, None);
        set_config_value_in(&path, "autoSend", serde_json::Value::Bool(false)).unwrap();
        let config = load_config_from(&path);
        assert_eq!(config.auto_send, Some(false));
        assert_eq!(config.ingest_token.as_deref(), Some("abc"));
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["retention"]["mediaDays"], 7);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn video_export_flag_defaults_off() {
        let fixture = Fixture::new();
        let path = fixture.0.join("config.json");
        fs::write(&path, br#"{"autoSend":false}"#).unwrap();
        assert_eq!(load_config_from(&path).video_export, None);
        fs::write(&path, br#"{"videoExport":true}"#).unwrap();
        assert_eq!(load_config_from(&path).video_export, Some(true));
    }

    #[test]
    fn sessions_read_their_directory_from_v2_location_or_v1_field() {
        let v2: OpenCodeSession = serde_json::from_str(
            r#"{"id":"ses_a","title":"A","time":{"updated":1},"location":{"directory":"/p/a"}}"#,
        )
        .unwrap();
        assert_eq!(v2.directory(), Some("/p/a"));
        assert!(v2.is_open_root());
        let v1: OpenCodeSession =
            serde_json::from_str(r#"{"id":"ses_b","parentID":"ses_a","directory":"/p/b"}"#).unwrap();
        assert_eq!(v1.directory(), Some("/p/b"));
        assert!(!v1.is_open_root());
        assert_eq!(v1.label(), "ses_b");
    }

    #[test]
    fn config_defaults_when_missing_or_minimal() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("missing.json");
        let cfg = load_config_from(&missing);
        assert_eq!(cfg, Config::default());
        assert!(cfg.usb_apps.is_empty());

        let token = ensure_ingest_token_in(&missing).unwrap();
        assert_eq!(token.len(), 64);
        let cfg_after = load_config_from(&missing);
        assert_eq!(cfg_after.ingest_token.as_deref(), Some(token.as_str()));
        assert!(cfg_after.usb_apps.is_empty());
    }
}

#[cfg(test)]
mod sync_live_check {
    /// Manual: `cargo test sync_to_pro -- --ignored` copies a dummy bundle to `pro` and checks the
    /// heavy media lands atomically and the marker clears.
    #[test]
    #[ignore]
    fn sync_to_pro() {
        let base = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Caches/scrawl-sync-live");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = base.join("scrawl-synctest.scrawl");
        std::fs::create_dir_all(bundle.join("frames-full")).unwrap();
        std::fs::write(bundle.join("summary.md"), "hi").unwrap();
        std::fs::write(bundle.join("recording.mp4"), vec![7u8; 8_000_000]).unwrap();
        std::fs::write(bundle.join("frames-full/frame-01.png"), b"png").unwrap();
        super::sync_bundle_to("pro", &bundle).unwrap();
        let q = super::shell_quote(&bundle.to_string_lossy());
        let check = format!("ls -a {q}; ls {q}/frames-full; wc -c < {q}/recording.mp4");
        let mut out = String::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let o = super::ssh("pro").arg(&check).output().unwrap();
            out = String::from_utf8_lossy(&o.stdout).into_owned();
            if !out.contains(super::MEDIA_PENDING) && out.contains("8000000") {
                break;
            }
        }
        let _ = super::ssh("pro").arg(format!("rm -rf {}", super::shell_quote(&base.to_string_lossy()))).status();
        assert!(out.contains("8000000") && out.contains("frame-01.png") && !out.contains(super::MEDIA_PENDING), "{out}");
    }
}
