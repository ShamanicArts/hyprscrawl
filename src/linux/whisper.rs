//! whisper.cpp (`whisper-cli`) transcription: the Linux default engine.
//!
//! Finds `whisper-cli` (`$SCRAWL_WHISPER_CLI`, `~/.local/share/scrawl/bin`, then `PATH`, which
//! covers Arch's `whisper-cpp` package) and a ggml model (`$SCRAWL_WHISPER_MODEL`,
//! `~/.local/share/scrawl/models/*.bin`, then Voxtype's `~/.local/share/voxtype/models/ggml-*.bin`,
//! read-only). Token offsets from `--output-json-full` are merged into words.

use std::path::{Path, PathBuf};
use anyhow::{Context, Result, bail};

use crate::model::{TranscriptData, TranscriptSegment, WordTiming};

fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("scrawl")
}

pub fn cli() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SCRAWL_WHISPER_CLI") {
        // An override is final: a wrong path means "none", not "look elsewhere".
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("whisper-cli")))
        .filter(|p| p.is_file())
    {
        return Some(beside);
    }
    let local = data_dir().join("bin/whisper-cli");
    if local.is_file() {
        return Some(local);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("whisper-cli"))
        .find(|p| p.is_file())
}

fn bins_in(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "bin")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    // Prefer the smaller English models: base.en before small/medium, which are slow on CPU.
    found.sort_by_key(|p| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
        let rank = ["base.en", "small.en", "base", "small", "tiny.en", "tiny"]
            .iter()
            .position(|k| n.contains(&format!("-{k}.")) || n.contains(&format!("-{k}-")))
            .unwrap_or(9);
        (rank, n)
    });
    found
}

pub fn model() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SCRAWL_WHISPER_MODEL") {
        // An override is final: a wrong path means "none", not "look elsewhere".
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    let home = dirs::home_dir()?;
    bins_in(&data_dir().join("models"), "")
        .into_iter()
        .chain(bins_in(&home.join(".local/share/voxtype/models"), "ggml-"))
        .next()
}

/// whisper.cpp's DTW alignment-head preset for a model file (`ggml-base.en.bin` → `base.en`).
pub fn dtw_preset(model: &Path) -> Option<String> {
    let stem = model.file_stem()?.to_str()?;
    let name = stem.strip_prefix("ggml-").unwrap_or(stem);
    let name = name.split("-q").next().unwrap_or(name);
    const KNOWN: [&str; 13] = [
        "tiny", "tiny.en", "base", "base.en", "small", "small.en", "medium", "medium.en",
        "large.v1", "large.v2", "large.v3", "large.v3.turbo", "large.v3.turbo",
    ];
    let name = name.replace("large-v3-turbo", "large.v3.turbo").replace("large-v", "large.v");
    KNOWN.contains(&name.as_str()).then_some(name)
}

/// DTW token times land ~130 ms after the word starts (measured on a TTS clip with known word
/// boundaries, FINDINGS.md); move them back by this much.
fn dtw_shift_ms() -> i64 {
    std::env::var("SCRAWL_WHISPER_DTW_SHIFT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

fn is_noise(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
        || (t.starts_with('*') && t.ends_with('*'))
}

pub fn transcribe(audio: &Path) -> Result<TranscriptData> {
    let cli = cli().context(
        "whisper-cli not found: install it with `omarchy-pkg-add whisper-cpp` (or set SCRAWL_WHISPER_CLI)",
    )?;
    let model = model().context(
        "No whisper model: run `scrawl setup transcription`, or set SCRAWL_WHISPER_MODEL",
    )?;
    let tmp = std::env::temp_dir().join(format!("scrawl-whisper-{}", crate::make_bundle_id()));
    std::fs::create_dir_all(&tmp)?;
    let of = tmp.join("out");
    let threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 8))
        .unwrap_or(4)
        .to_string();
    let started = std::time::Instant::now();
    let dtw = dtw_preset(&model);
    let mut cmd = std::process::Command::new("nice");
    cmd.args(["-n", "10"])
        .arg(&cli)
        .arg("-m")
        .arg(&model)
        .arg("-f")
        .arg(audio)
        .args(["-t", &threads, "--output-json-full", "--no-prints"]);
    if let Some(preset) = &dtw {
        // DTW is silently skipped while flash attention is on (whisper.cpp's default).
        cmd.args(["-nfa", "--dtw", preset]);
    }
    let status = cmd
        .arg("-of")
        .arg(&of)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("Could not run {}", cli.display()))?;
    if !status.status.success() {
        let err = String::from_utf8_lossy(&status.stderr);
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("whisper-cli failed: {}", err.lines().rev().take(4).collect::<Vec<_>>().join(" | "));
    }
    let json_path = of.with_extension("json");
    let raw = std::fs::read(&json_path).with_context(|| format!("No {}", json_path.display()))?;
    let _ = std::fs::remove_dir_all(&tmp);
    let model_name = model
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("whisper")
        .to_owned();
    let data = parse(&raw, &model_name, dtw_shift_ms())?;
    crate::capture::scrawl_log(format!(
        "whisper.cpp {model_name}: {} words in {} ms",
        data.words.len(),
        started.elapsed().as_millis()
    ));
    Ok(data)
}

/// Parses `whisper-cli --output-json-full` output.
pub fn parse(raw: &[u8], model_name: &str, dtw_shift_ms: i64) -> Result<TranscriptData> {
    let v: serde_json::Value = serde_json::from_slice(raw).context("Invalid whisper JSON")?;
    let mut segments = Vec::new();
    let mut words: Vec<WordTiming> = Vec::new();
    // Measured (DTW) times when every spoken token has one; otherwise whisper's token offsets,
    // which run hundreds of ms early, so the words are marked estimated.
    let mut measured = true;
    for seg in v["transcription"].as_array().into_iter().flatten() {
        let text = seg["text"].as_str().unwrap_or("").trim().to_owned();
        if is_noise(&text) {
            continue;
        }
        let t0 = seg["offsets"]["from"].as_u64().unwrap_or(0);
        let t1 = seg["offsets"]["to"].as_u64().unwrap_or(t0);
        segments.push(TranscriptSegment { t0, t1, text });
        let mut current: Option<WordTiming> = None;
        for tok in seg["tokens"].as_array().into_iter().flatten() {
            let text = tok["text"].as_str().unwrap_or("");
            if text.starts_with("[_") || text.starts_with("<|") || text.is_empty() {
                continue;
            }
            let dtw = tok["t_dtw"].as_i64().filter(|&d| d >= 0);
            if dtw.is_none() {
                measured = false;
            }
            let a = match dtw {
                Some(d) => (d * 10 - dtw_shift_ms).max(0) as u64,
                None => tok["offsets"]["from"].as_u64().unwrap_or(t0),
            };
            let b = tok["offsets"]["to"].as_u64().unwrap_or(a).max(a);
            let starts_word = text.starts_with(' ')
                || current.is_none()
                || !text.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '\'');
            let punct_only = text.trim().chars().all(|c| !c.is_alphanumeric());
            match current.as_mut() {
                Some(w) if !starts_word || (punct_only && !text.starts_with(' ')) => {
                    w.text.push_str(text);
                    w.t1 = w.t1.max(b);
                }
                _ => {
                    if let Some(w) = current.take() {
                        words.push(w);
                    }
                    current = Some(WordTiming {
                        t0: a,
                        t1: b.max(a),
                        text: text.trim_start().to_owned(),
                    });
                }
            }
        }
        if let Some(w) = current.take() {
            words.push(w);
        }
    }
    words.retain(|w| !w.text.trim().is_empty());
    for w in &mut words {
        w.text = w.text.trim().to_owned();
    }
    if measured {
        // DTW gives starts; a word ends where the next begins (capped at 1.2 s).
        for i in 0..words.len() {
            let next = words.get(i + 1).map(|n| n.t0);
            let w = &mut words[i];
            w.t1 = next.map_or(w.t1.max(w.t0 + 200), |n| n.max(w.t0)).min(w.t0 + 1_200);
        }
    }
    let text = segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(TranscriptData {
        source: "whisper.cpp".into(),
        model: Some(model_name.to_owned()),
        text,
        segments,
        estimated: !measured,
        words,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_tokens_into_words() {
        let raw = br#"{"transcription":[{"offsets":{"from":0,"to":2000},"text":" Click here, now.",
          "tokens":[{"text":"[_BEG_]","offsets":{"from":0,"to":0}},
                    {"text":" Click","offsets":{"from":100,"to":400}},
                    {"text":" he","offsets":{"from":450,"to":600}},
                    {"text":"re","offsets":{"from":600,"to":700}},
                    {"text":",","offsets":{"from":700,"to":710}},
                    {"text":" now","offsets":{"from":900,"to":1200}},
                    {"text":".","offsets":{"from":1200,"to":1210}}]},
          {"offsets":{"from":2000,"to":3000},"text":" [BLANK_AUDIO]","tokens":[]}]}"#;
        let t = parse(raw, "ggml-base.en", 100).unwrap();
        let w: Vec<(&str, u64, u64)> = t.words.iter().map(|w| (w.text.as_str(), w.t0, w.t1)).collect();
        assert_eq!(w, vec![("Click", 100, 400), ("here,", 450, 710), ("now.", 900, 1210)]);
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.text, "Click here, now.");
        assert!(t.estimated, "token offsets alone are estimated");
    }

    #[test]
    fn dtw_times_are_measured_and_shifted() {
        let raw = br#"{"transcription":[{"offsets":{"from":0,"to":2000},"text":" look here",
          "tokens":[{"text":" look","offsets":{"from":0,"to":400},"t_dtw":120},
                    {"text":" here","offsets":{"from":400,"to":900},"t_dtw":180}]}]}"#;
        let t = parse(raw, "ggml-base.en", 100).unwrap();
        let w: Vec<(&str, u64, u64)> = t.words.iter().map(|w| (w.text.as_str(), w.t0, w.t1)).collect();
        assert_eq!(w, vec![("look", 1100, 1700), ("here", 1700, 1900)]);
        assert!(!t.estimated);
    }

    #[test]
    fn dtw_presets() {
        use std::path::Path;
        assert_eq!(dtw_preset(Path::new("/m/ggml-base.en.bin")).as_deref(), Some("base.en"));
        assert_eq!(dtw_preset(Path::new("ggml-large-v3-turbo.bin")).as_deref(), Some("large.v3.turbo"));
        assert_eq!(dtw_preset(Path::new("ggml-small.en-q5_1.bin")).as_deref(), Some("small.en"));
        assert_eq!(dtw_preset(Path::new("custom.bin")), None);
    }
}
