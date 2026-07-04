//! WatchVideo — let a vision LLM *watch* a video. Extracts scene-aware,
//! deduplicated key frames (one ffmpeg pass: every scene change + a density
//! floor) and returns them as inline image blocks so the model sees the
//! pixels, plus an optional Groq Whisper transcript. The dedup (downscaled
//! RGB diff against a sliding window of recent kept frames) drops near-
//! duplicates and A-B-A cutaways, so a static screencast collapses to one
//! frame and a fast-cut reel keeps each change — far fewer, more meaningful
//! frames than fixed-interval sampling.
//!
//! Local files only (the sandbox gates the path). Needs `ffmpeg`/`ffprobe`
//! on PATH; the transcript needs `GROQ_API_KEY` (whisper-large-v3).

use super::read::downscale_for_vision;
use super::{req_str, Tool};
use crate::error::{Error, Result};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_MAX_FRAMES: usize = 32;
const DEDUP_WINDOW: usize = 4;

pub struct WatchVideoTool;

fn run(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    cmd.output()
}

fn ffprobe_f(video: &Path, entries: &str) -> Option<String> {
    let out = run(std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            entries,
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(video))
    .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn duration_secs(video: &Path) -> f64 {
    run(std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(video))
    .ok()
    .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
    .unwrap_or(0.0)
}

fn fps(video: &Path) -> f64 {
    ffprobe_f(video, "stream=avg_frame_rate")
        .and_then(|s| {
            let (n, d) = s.split_once('/')?;
            let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
            (d != 0.0).then_some(n / d)
        })
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(25.0)
}

/// 16×16 RGB signature for cheap pixel-diff dedup.
fn signature(path: &Path) -> Option<Vec<[u8; 3]>> {
    let img = image::open(path)
        .ok()?
        .resize_exact(16, 16, image::imageops::FilterType::Triangle);
    Some(img.to_rgb8().pixels().map(|p| p.0).collect())
}

/// % of pixels whose max channel delta exceeds `tol` — the same measure as
/// pixelmatch, robust to flat colours where a perceptual hash goes blind.
fn pct_diff(a: &[[u8; 3]], b: &[[u8; 3]]) -> f64 {
    let tol = 25i16;
    let changed = a
        .iter()
        .zip(b)
        .filter(|(x, y)| {
            (x[0] as i16 - y[0] as i16)
                .abs()
                .max((x[1] as i16 - y[1] as i16).abs())
                .max((x[2] as i16 - y[2] as i16).abs())
                > tol
        })
        .count();
    100.0 * changed as f64 / a.len().max(1) as f64
}

fn tmp_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "thclaws-watch-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(&d);
    d
}

async fn groq_transcript(video: &Path, dir: &Path, lang: Option<&str>) -> Option<String> {
    // BYOK-or-gateway (dev-plan/53 Stage D): a real GROQ_API_KEY posts
    // to Groq directly; a gateway key routes via `<gw>/groq/audio/…`
    // (per-second metered). Neither → skip the transcript, as before.
    let ep = crate::media::provider::resolve_endpoint(
        &["GROQ_API_KEY"],
        "https://api.groq.com/openai/v1",
        "groq",
    )
    .ok()?;
    // Skip cleanly if there's no audio stream.
    let has_audio = run(std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(video))
    .ok()
    .map(|o| !o.stdout.is_empty())
    .unwrap_or(false);
    if !has_audio {
        return None;
    }
    let wav = dir.join("audio.wav");
    run(std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(video)
        .args(["-vn", "-ar", "16000", "-ac", "1"])
        .arg(&wav)
        .args(["-hide_banner", "-loglevel", "error"]))
    .ok()?;
    let bytes = std::fs::read(&wav).ok()?;
    let mut form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .ok()?,
        )
        .text("model", "whisper-large-v3")
        .text("response_format", "text");
    if let Some(l) = lang.filter(|l| *l != "auto") {
        form = form.text("language", l.to_string());
    }
    let resp = crate::multi_tenant::attach_member(
        reqwest::Client::new().post(format!("{}/audio/transcriptions", ep.base_url)),
    )
    .bearer_auth(&ep.api_key)
    .multipart(form)
    .send()
    .await
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text()
        .await
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[async_trait]
impl Tool for WatchVideoTool {
    fn name(&self) -> &'static str {
        "WatchVideo"
    }

    fn description(&self) -> &'static str {
        "Watch a local video file: extracts scene-aware, deduplicated key frames \
         and returns them as inline images so you can SEE the video (not just its \
         transcript), plus a Whisper transcript when GROQ_API_KEY is set. Use it \
         to review/critique a video, check a generated clip, or answer questions \
         about what happens on screen. Args: path (required), scene (0-1 \
         sensitivity, lower=more frames, default 0.3), fps_floor (>=1 frame every \
         N sec, default 1.0), max_frames (default 32), dedup (% pixels changed to \
         count as new, default 8), lang (Whisper language, default auto)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to a local video file"},
                "scene": {"type": "number", "description": "Scene-change sensitivity 0-1 (default 0.3)"},
                "fps_floor": {"type": "number", "description": "At least one frame every N seconds (default 1.0)"},
                "max_frames": {"type": "integer", "description": "Cap on frames returned (default 32)"},
                "dedup": {"type": "number", "description": "% of pixels that must change for a new frame (default 8)"},
                "lang": {"type": "string", "description": "Whisper language e.g. th/en/auto (default auto)"}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        // Text-only fallback: real output is images, via call_multimodal.
        let _ = req_str(&input, "path")?;
        Ok(
            "WatchVideo returns image frames — invoke it via the agent loop (call_multimodal)."
                .into(),
        )
    }

    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let raw = req_str(&input, "path")?;
        let video = crate::sandbox::Sandbox::check(raw)?;
        if crate::filmscript::harness::check_av_tools().is_err() {
            return Err(Error::Tool("ffmpeg/ffprobe not found on PATH".into()));
        }
        let scene = input.get("scene").and_then(Value::as_f64).unwrap_or(0.30);
        let fps_floor = input
            .get("fps_floor")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .max(0.1);
        let max_frames = input
            .get("max_frames")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_FRAMES as u64) as usize;
        let dedup = input.get("dedup").and_then(Value::as_f64).unwrap_or(8.0);
        let lang = input.get("lang").and_then(Value::as_str);

        let dir = tmp_dir();
        let dur = duration_secs(&video);
        let every_n = (fps(&video) * fps_floor).round().max(1.0) as u64;

        // One chronological pass: scene changes OR a density floor.
        let status = run(std::process::Command::new("ffmpeg")
            .args(["-i"])
            .arg(&video)
            .args([
                "-vf",
                &format!("select='gt(scene,{scene})+not(mod(n,{every_n}))',scale=640:-2"),
                "-vsync",
                "vfr",
            ])
            .arg(dir.join("raw_%05d.jpg"))
            .args(["-hide_banner", "-loglevel", "error"]))
        .map_err(|e| Error::Tool(format!("ffmpeg: {e}")))?;
        if !status.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Error::Tool(format!(
                "ffmpeg frame extraction failed: {}",
                String::from_utf8_lossy(&status.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }

        let mut raw: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| Error::Tool(format!("read frames: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "jpg").unwrap_or(false))
            .collect();
        raw.sort();
        let extracted = raw.len();

        // Dedup against a sliding window of recent kept signatures.
        let mut kept: Vec<PathBuf> = Vec::new();
        let mut recent: Vec<Vec<[u8; 3]>> = Vec::new();
        for p in raw {
            let Some(sig) = signature(&p) else { continue };
            let dup = recent.iter().any(|r| pct_diff(&sig, r) <= dedup);
            if !dup {
                recent.push(sig);
                if recent.len() > DEDUP_WINDOW {
                    recent.remove(0);
                }
                kept.push(p);
            }
        }
        // Cap: thin uniformly so survivors stay spread across the video.
        if kept.len() > max_frames && max_frames > 0 {
            let step = kept.len() as f64 / max_frames as f64;
            let keep: std::collections::BTreeSet<usize> = (0..max_frames)
                .map(|i| (i as f64 * step) as usize)
                .collect();
            kept = kept
                .into_iter()
                .enumerate()
                .filter(|(i, _)| keep.contains(i))
                .map(|(_, p)| p)
                .collect();
        }

        let transcript = groq_transcript(&video, &dir, lang).await;

        // Build the result: a summary + each kept frame as an image block.
        let mut blocks: Vec<ToolResultBlock> = Vec::new();
        let name = video
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video");
        let mut summary = format!(
            "Watched {name} — {:.0}s, {} key frames (scene-aware, deduped from {} extracted). \
             Frames are in chronological order below.",
            dur,
            kept.len(),
            extracted
        );
        let transcribable = std::env::var("GROQ_API_KEY").is_ok()
            || crate::providers::thclaws_gateway::has_access_key();
        match &transcript {
            Some(t) => summary.push_str(&format!("\n\n--- transcript (whisper-large-v3) ---\n{t}")),
            None if transcribable => {
                summary.push_str("\n\n(no transcript — the video has no audio)")
            }
            None => summary.push_str(
                "\n\n(no transcript — set GROQ_API_KEY or enable the thClaws Gateway to transcribe the audio)",
            ),
        }
        blocks.push(ToolResultBlock::Text { text: summary });

        for p in &kept {
            let Ok(bytes) = std::fs::read(p) else {
                continue;
            };
            let (out, mime): (Vec<u8>, &str) = match downscale_for_vision(&bytes, "image/jpeg") {
                Ok(Some((b, m))) => (b, m),
                _ => (bytes, "image/jpeg"),
            };
            blocks.push(ToolResultBlock::Image {
                source: ImageSource::Base64 {
                    media_type: mime.to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&out),
                },
            });
        }

        let _ = std::fs::remove_dir_all(&dir);
        if kept.is_empty() {
            return Err(Error::Tool(
                "no frames extracted — is this a valid video?".into(),
            ));
        }
        Ok(ToolResultContent::Blocks(blocks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_diff_bounds() {
        let black = vec![[0u8, 0, 0]; 256];
        let white = vec![[255u8, 255, 255]; 256];
        assert_eq!(pct_diff(&black, &black), 0.0);
        assert!(pct_diff(&black, &white) > 99.0);
    }

    // Live: WatchVideo on a real clip → image blocks + Groq transcript.
    // `cargo test watch_reel_clip_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn watch_reel_clip_live() {
        let path = std::env::var("WATCH_CLIP").unwrap_or_else(|_| {
            "/Volumes/Data01/agentic-workspace/dev-plan/52-ltx-lab/reel/clips/grok/shot-4.mp4"
                .into()
        });
        let r = WatchVideoTool
            .call_multimodal(json!({ "path": path, "lang": "th", "max_frames": 8 }))
            .await
            .expect("watch");
        let ToolResultContent::Blocks(b) = r else {
            panic!("expected blocks")
        };
        let imgs = b
            .iter()
            .filter(|x| matches!(x, ToolResultBlock::Image { .. }))
            .count();
        let summary = b
            .iter()
            .find_map(|x| match x {
                ToolResultBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        println!("\n=== {imgs} image blocks ===\n{summary}\n");
        assert!(imgs > 0, "no frames returned");
    }
}
