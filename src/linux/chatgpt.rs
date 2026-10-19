//! ChatGPT's transcription, the endpoint ChatGPT/Codex voice input uses, signed in with the Codex
//! CLI's ChatGPT login (`$CODEX_HOME/auth.json`, default `~/.codex/auth.json`). Opt-in, because
//! the audio leaves the machine: `"transcriber": "chatgpt"` in the config, or
//! `SCRAWL_TRANSCRIBER=chatgpt`.
//!
//! The endpoint returns text only. Scrawl needs word times (keyframes take their words from them,
//! and spoken cues cut keyframes), so local whisper.cpp runs alongside when it is installed and
//! the cloud words are aligned onto whisper's DTW word times. Without whisper, or when the two
//! disagree too much, words are spread over the speech islands like upstream's Hex engine and
//! marked `estimated`.
//!
//! Nothing here logs or prints the token; it reaches `curl` on stdin, not in its arguments.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::model::{TranscriptData, TranscriptSegment, WordTiming};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
pub const MODEL: &str = "gpt-transcribe";
/// The service's upload cap is 20 MB; longer recordings go up in speech-island clips.
const MAX_UPLOAD_BYTES: u64 = 19 * 1024 * 1024;
/// The longest clip sent in one request. Clips go up as 16 kHz 16-bit mono (32 bytes per ms), so
/// five minutes plus padding is about 10 MB, well under the cap.
const MAX_CLIP_MS: u64 = 5 * 60_000;
/// Below this share of cloud words found in whisper's words, the alignment isn't trusted.
const MIN_ALIGNED: f64 = 0.6;

/// ChatGPT is chosen in the config or `SCRAWL_TRANSCRIBER`. A `--transcriber` on the command line
/// (`reprocess`, `import`) names an upstream engine, which on Linux means whisper.cpp: no upload.
pub fn enabled() -> bool {
    !crate::transcribe::overridden() && std::env::var("SCRAWL_TRANSCRIBER")
        .ok()
        .or_else(crate::handoff::transcriber)
        .is_some_and(|name| name.trim().eq_ignore_ascii_case("chatgpt"))
}

pub fn auth_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
        .join("auth.json")
}

struct Auth {
    token: String,
    account: String,
}

fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn jwt_exp(token: &str) -> Option<u64> {
    let bytes = base64url(token.split('.').nth(1)?)?;
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()?["exp"].as_u64()
}

fn auth() -> Result<Auth> {
    let path = auth_path();
    let raw = std::fs::read(&path)
        .with_context(|| format!("No Codex login at {} (run `codex login`)", path.display()))?;
    let json: serde_json::Value = serde_json::from_slice(&raw).context("Codex auth.json is not JSON")?;
    let tokens = &json["tokens"];
    let token = tokens["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .context("Codex auth.json has no ChatGPT login (an API key login won't do; run `codex login`)")?;
    let account = tokens["account_id"]
        .as_str()
        .filter(|a| !a.is_empty())
        .context("Codex auth.json has no ChatGPT account id")?;
    if let Some(exp) = jwt_exp(token) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        ensure!(now < exp, "The Codex login has expired: run any `codex` command to refresh it");
    }
    Ok(Auth {
        token: token.to_owned(),
        account: account.to_owned(),
    })
}

/// Whether a ChatGPT login is there to use, with the reason when not.
pub fn status() -> Result<()> {
    auth().map(|_| ())
}

/// One request: the text the service heard in `wav`.
fn request(auth: &Auth, wav: &Path) -> Result<String> {
    let mut child = Command::new("curl")
        // Cloudflare turns away curl's own User-Agent.
        .arg("-A")
        .arg(format!("scrawl/{}", env!("CARGO_PKG_VERSION")))
        .args(["-sS", "--max-time", "120", "-K", "-", "-w", "\n%{http_code}"])
        .arg("-F")
        .arg(format!("file=@{};type=audio/wav", wav.display()))
        .args(["-F", &format!("model={MODEL}"), ENDPOINT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not run curl")?;
    let config = format!(
        "header = \"Authorization: Bearer {}\"\nheader = \"chatgpt-account-id: {}\"\nheader = \"originator: scrawl\"\n",
        auth.token, auth.account
    );
    child.stdin.take().context("curl stdin")?.write_all(config.as_bytes())?;
    let out = child.wait_with_output()?;
    let body = String::from_utf8_lossy(&out.stdout);
    let (body, code) = body.rsplit_once('\n').unwrap_or(("", &body));
    if !out.status.success() {
        bail!("curl failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if code != "200" {
        let detail: String = body.chars().take(200).collect();
        bail!("ChatGPT transcription answered {code}: {detail}");
    }
    let json: serde_json::Value = serde_json::from_str(body).context("ChatGPT transcription sent no JSON")?;
    Ok(json["text"].as_str().context("ChatGPT transcription sent no text")?.trim().to_owned())
}

pub fn transcribe(wav: &Path) -> Result<TranscriptData> {
    start(wav)?.finish(whisper_grace())
}

/// How long the transcript may wait for whisper's timings once ChatGPT's text is in hand and
/// the transcript is wanted (`SCRAWL_WHISPER_GRACE_MS`, default 3 s). Past that, whisper is
/// stopped and the words are spread as estimates.
pub fn whisper_grace() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("SCRAWL_WHISPER_GRACE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(3_000),
    )
}

type Whisper = (std::thread::JoinHandle<Result<TranscriptData>>, std::sync::Arc<std::sync::atomic::AtomicBool>);

/// A transcription under way: ChatGPT's request and whisper (for timings) both running.
pub struct Pending {
    started: std::time::Instant,
    /// ChatGPT's text and how long it took to arrive.
    cloud: std::thread::JoinHandle<Result<(String, u128)>>,
    whisper: Option<Whisper>,
    islands: Vec<(u64, u64)>,
    duration_ms: u64,
}

/// Starts ChatGPT and whisper on `wav` and returns at once.
pub fn start(wav: &Path) -> Result<Pending> {
    let auth = auth()?;
    let started = std::time::Instant::now();
    let bytes = std::fs::read(wav).context("Could not read the recording")?;
    let samples = crate::audio::decode_wav_16k(&bytes)?;
    let duration_ms = samples.len() as u64 / 16;
    let islands = crate::audio::speech_islands(&samples);
    let whisper = (crate::linux::whisper::cli().is_some() && crate::linux::whisper::model().is_some()).then(|| {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (wav, flag) = (wav.to_owned(), std::sync::Arc::clone(&cancel));
        (std::thread::spawn(move || crate::linux::whisper::transcribe_cancellable(&wav, &flag)), cancel)
    });
    let cloud = {
        let (wav, islands) = (wav.to_owned(), islands.clone());
        std::thread::spawn(move || -> Result<(String, u128)> {
            if bytes.len() as u64 <= MAX_UPLOAD_BYTES {
                return request(&auth, &wav).map(|text| (text, started.elapsed().as_millis()));
            }
            let clips = bounded_clips(crate::hex::plan_clips(&islands, duration_ms), duration_ms);
            crate::capture::scrawl_log(format!(
                "chatgpt: {} MB is over the upload cap; sending {} clips, the longest {} s",
                bytes.len() >> 20,
                clips.len(),
                clips.iter().map(|(a, b)| b - a).max().unwrap_or(0) / 1_000
            ));
            let dir = crate::linux::gsr::runtime_dir();
            std::fs::create_dir_all(&dir)?;
            let clip_path = dir.join(format!("chatgpt-clip-{}.wav", std::process::id()));
            // Each clip's own answer, in order, to stitch the cut seams (Hex's join can't know them).
            let mut texts = Vec::new();
            let result = crate::hex::transcribe_clips(&samples, &clips, MODEL, |clip| {
                std::fs::write(&clip_path, clip).map_err(|e| crate::hex::Failure::Unavailable(e.to_string()))?;
                let text = request(&auth, &clip_path).map_err(|e| crate::hex::Failure::Unavailable(format!("{e:#}")))?;
                texts.push(text.clone());
                Ok(text)
            });
            let _ = std::fs::remove_file(&clip_path);
            let joined = result?.text;
            let text = if texts.len() == clips.len() { stitch(&clips, &texts) } else { joined };
            Ok((text, started.elapsed().as_millis()))
        })
    };
    Ok(Pending {
        started,
        cloud,
        whisper,
        islands,
        duration_ms,
    })
}

impl Pending {
    /// ChatGPT's text, timed by whisper if whisper is done within `grace` of the text.
    pub fn finish(self, grace: std::time::Duration) -> Result<TranscriptData> {
        let cancel_whisper = |whisper: Option<Whisper>| {
            if let Some((handle, cancel)) = whisper {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = handle.join();
            }
        };
        let (text, cloud_ms) = match self.cloud.join() {
            Ok(Ok(answer)) => answer,
            Ok(Err(error)) => {
                cancel_whisper(self.whisper);
                return Err(error);
            }
            Err(_) => {
                cancel_whisper(self.whisper);
                bail!("ChatGPT request thread panicked");
            }
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut transcript = TranscriptData {
            source: format!("chatgpt:{MODEL}"),
            model: Some(MODEL.into()),
            ..Default::default()
        };
        if text.is_empty() {
            cancel_whisper(self.whisper);
            crate::capture::scrawl_log(format!("chatgpt {MODEL}: no speech in {cloud_ms} ms"));
            return Ok(transcript);
        }
        let deadline = std::time::Instant::now() + grace;
        let mut late = false;
        let reference = self.whisper.and_then(|(handle, cancel)| {
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if !handle.is_finished() {
                late = true;
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            match handle.join() {
                Ok(Ok(data)) if !data.estimated && !data.words.is_empty() => Some(data.words),
                Ok(Err(error)) if !late => {
                    crate::capture::scrawl_log(format!("whisper (timing only) failed: {error:#}"));
                    None
                }
                _ => None,
            }
        });
        let aligned = reference.as_deref().and_then(|words| align(&text, words));
        transcript.words = match aligned {
            Some(words) => words,
            None => {
                transcript.estimated = true;
                spread_over_islands(&text, &self.islands, self.duration_ms)
            }
        };
        transcript.segments = segments_of(&transcript.words);
        transcript.text = text;
        crate::capture::scrawl_log(format!(
            "chatgpt {MODEL}: {} words, cloud {cloud_ms} ms, total {} ms, times {}",
            transcript.words.len(),
            self.started.elapsed().as_millis(),
            if !transcript.estimated {
                "aligned to whisper"
            } else if late {
                "estimated (whisper too slow, stopped)"
            } else {
                "estimated"
            }
        ));
        Ok(transcript)
    }
}

/// Clips small enough to upload: Hex's plan (or the whole recording when it found no speech),
/// with anything longer than `MAX_CLIP_MS` cut into equal parts.
fn bounded_clips(planned: Vec<(u64, u64)>, duration_ms: u64) -> Vec<(u64, u64)> {
    let planned = if planned.is_empty() { vec![(0, duration_ms)] } else { planned };
    let mut clips = Vec::new();
    for (start, end) in planned {
        let parts = (end.saturating_sub(start)).div_ceil(MAX_CLIP_MS).max(1);
        let step = (end - start).div_ceil(parts);
        let mut from = start;
        while from < end {
            let to = (from + step).min(end);
            clips.push((from, to));
            from = to;
        }
    }
    clips
}

/// At most this many words are taken as heard twice across a cut (the 400 ms both clips share).
const SEAM_WORDS: usize = 3;

/// Joins the clips' answers. Where `bounded_clips` cut through speech (the clips touch), both
/// requests heard the padding around the cut, so words that end one answer and start the next
/// are kept once.
fn stitch(clips: &[(u64, u64)], texts: &[String]) -> String {
    let mut words: Vec<&str> = Vec::new();
    // How many of `words` came from the previous answer: only those can have been heard twice.
    let mut previous = 0;
    for (i, text) in texts.iter().enumerate() {
        let mut next: Vec<&str> = text.split_whitespace().collect();
        let fresh = next.len();
        if i > 0 && clips[i - 1].1 == clips[i].0 {
            let most = SEAM_WORDS.min(next.len()).min(previous);
            let repeated = (1..=most)
                .rev()
                .find(|&k| words[words.len() - k..].iter().map(|w| norm(w)).eq(next[..k].iter().map(|w| norm(w))))
                .unwrap_or(0);
            next.drain(..repeated);
        }
        previous = fresh;
        words.extend(next);
    }
    words.join(" ")
}

fn norm(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Gives each word of `text` a time from `reference` (whisper's words): the longest common
/// subsequence of normalised words anchors the matches, and the words between two anchors are
/// spread over the gap between them. `None` when too few words match to trust it.
pub fn align(text: &str, reference: &[WordTiming]) -> Option<Vec<WordTiming>> {
    let cloud: Vec<&str> = text.split_whitespace().collect();
    let a: Vec<String> = cloud.iter().map(|w| norm(w)).collect();
    let b: Vec<String> = reference.iter().map(|w| norm(&w.text)).collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 || n.saturating_mul(m) > 30_000_000 {
        return None;
    }
    // lcs[i][j] = LCS length of a[i..] and b[j..].
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if !a[i].is_empty() && a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let mut matched: Vec<Option<usize>> = vec![None; n];
    let (mut i, mut j, mut count) = (0, 0, 0usize);
    while i < n && j < m {
        if !a[i].is_empty() && a[i] == b[j] {
            matched[i] = Some(j);
            count += 1;
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    if (count as f64) < MIN_ALIGNED * n as f64 {
        return None;
    }

    let mut words: Vec<WordTiming> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        if let Some(j) = matched[i] {
            words.push(WordTiming {
                t0: reference[j].t0,
                t1: reference[j].t1,
                text: cloud[i].to_owned(),
            });
            i += 1;
            continue;
        }
        // A run of unmatched words: fill the gap between its neighbouring anchors.
        let run_end = (i..n).find(|&k| matched[k].is_some()).unwrap_or(n);
        let run = cloud[i..run_end].join(" ");
        let guess = 280 * (run_end - i) as u64;
        let start = words.last().map(|w| w.t1);
        let end = matched.get(run_end).copied().flatten().map(|j| reference[j].t0);
        let (from, to) = match (start, end) {
            (Some(s), Some(e)) => (s, e.max(s)),
            (Some(s), None) => (s, s + guess),
            (None, Some(e)) => (e.saturating_sub(guess), e),
            (None, None) => (0, guess),
        };
        words.extend(crate::hex::spread_words(&run, from, to).1);
        i = run_end;
    }
    Some(words)
}

/// Spreads `text` over the speech islands as if they were one span, then maps the times back,
/// so no word lands in a silence. The whole recording stands in when no island was found.
pub fn spread_over_islands(text: &str, islands: &[(u64, u64)], duration_ms: u64) -> Vec<WordTiming> {
    let islands: Vec<(u64, u64)> = islands.iter().copied().filter(|(a, b)| b > a).collect();
    let islands = if islands.is_empty() { vec![(0, duration_ms.max(1))] } else { islands };
    let total: u64 = islands.iter().map(|(a, b)| b - a).sum();
    let real = |v: u64, at_end: bool| {
        let mut left = v;
        for &(a, b) in &islands {
            let len = b - a;
            if left < len || (at_end && left == len) {
                return a + left;
            }
            left -= len;
        }
        islands.last().map_or(v, |&(_, b)| b)
    };
    crate::hex::spread_words(text, 0, total)
        .1
        .into_iter()
        .map(|w| WordTiming {
            t0: real(w.t0, false),
            t1: real(w.t1, true),
            text: w.text,
        })
        .collect()
}

fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', '”', '’', ')', ']'])
        .ends_with(['.', '!', '?', ';', ':'])
}

/// Sentences, as upstream's segments are.
pub fn segments_of(words: &[WordTiming]) -> Vec<TranscriptSegment> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (index, word) in words.iter().enumerate() {
        if ends_sentence(&word.text) || index + 1 == words.len() {
            let phrase = &words[start..=index];
            segments.push(TranscriptSegment {
                t0: phrase[0].t0,
                t1: phrase.iter().map(|w| w.t1).max().unwrap_or(phrase[0].t1),
                text: phrase.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "),
            });
            start = index + 1;
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    #[test]
    fn cut_seams_keep_a_twice_heard_word_once() {
        let texts = ["now press Save.".to_string(), "save and continue".to_string(), "Save again".to_string()];
        // A cut between the first two; a real gap before the third.
        let clips = [(0, 300_000), (300_000, 400_000), (500_000, 501_000)];
        assert_eq!(stitch(&clips, &texts), "now press Save. and continue Save again");
        // Nothing repeated at the cut: nothing dropped.
        let texts = ["one two".to_string(), "three four".to_string()];
        assert_eq!(stitch(&clips[..2], &texts), "one two three four");
        // Only the answer just before a cut can repeat across it, never an earlier clip's words.
        let clips = [(0, 1_000), (10_000, 310_000), (310_000, 610_000)];
        let texts = ["Save.".to_string(), String::new(), "Save and continue".to_string()];
        assert_eq!(stitch(&clips, &texts), "Save. Save and continue");
        let texts = ["press Save".to_string(), "ok".to_string(), "press Save now".to_string()];
        assert_eq!(stitch(&clips, &texts), "press Save ok press Save now");
    }

    #[test]
    fn long_clips_are_split_under_the_upload_cap() {
        let hour = 60 * 60_000;
        // No speech found: the whole recording, in parts.
        let whole = bounded_clips(Vec::new(), hour);
        assert_eq!(whole.len(), 12);
        // One long island next to a short one.
        let clips = bounded_clips(vec![(1_000, 2_000), (10_000, 10_000 + 11 * 60_000)], hour);
        assert_eq!(clips[0], (1_000, 2_000));
        assert_eq!(clips.len(), 4);
        for clips in [&whole, &clips] {
            for &(a, b) in clips.iter() {
                assert!(b > a && b - a <= MAX_CLIP_MS);
                // Encoded with the padding on both sides, as hex::transcribe_clips sends it.
                assert!(44 + (b - a + 400) * 32 <= MAX_UPLOAD_BYTES);
            }
        }
        assert_eq!(clips.last().unwrap().1, 10_000 + 11 * 60_000);
        assert!(clips.windows(2).skip(1).all(|w| w[0].1 == w[1].0), "the long island is cut without gaps");
        assert_eq!(bounded_clips(vec![(0, 5_000)], 5_000), vec![(0, 5_000)]);
    }

    use super::*;

    fn w(t0: u64, t1: u64, text: &str) -> WordTiming {
        WordTiming { t0, t1, text: text.into() }
    }

    #[test]
    fn cloud_words_take_whisper_times_and_fill_gaps() {
        let whisper = [w(100, 300, "Okay"), w(350, 500, "look"), w(900, 1100, "here."), w(1500, 1700, "Now")];
        // "at this" isn't in whisper's words; "Now," differs only by punctuation.
        let words = align("Okay, look at this here. Now,", &whisper).unwrap();
        let times: Vec<_> = words.iter().map(|w| (w.text.as_str(), w.t0, w.t1)).collect();
        assert_eq!(times[0], ("Okay,", 100, 300));
        assert_eq!(times[1], ("look", 350, 500));
        assert_eq!((times[2].1, times[3].2), (500, 900));
        assert_eq!(times[4], ("here.", 900, 1100));
        assert_eq!(times[5], ("Now,", 1500, 1700));
        assert_eq!(segments_of(&words).len(), 2);
    }

    #[test]
    fn disagreeing_transcripts_are_not_aligned() {
        let whisper = [w(0, 100, "completely"), w(100, 200, "different")];
        assert!(align("nothing in common here", &whisper).is_none());
    }

    #[test]
    fn jwt_expiry_is_read() {
        // {"exp":1700000000}
        assert_eq!(jwt_exp("x.eyJleHAiOjE3MDAwMDAwMDB9.y"), Some(1_700_000_000));
    }

    #[test]
    fn island_spreading_skips_silence() {
        let words = spread_over_islands("aa bb cc dd", &[(1000, 2000), (5000, 6000)], 8000);
        assert_eq!(words[0].t0, 1000);
        assert_eq!(words[1].t1, 2000);
        assert_eq!(words[2].t0, 5000);
        assert_eq!(words[3].t1, 6000);
    }
}
