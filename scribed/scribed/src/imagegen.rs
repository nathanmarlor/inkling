//! OpenRouter client (DESIGN.md §6.1). Single-step: sketch_to_illustration()
//! sends the rough sketch to the image model with STYLE_PROMPT and gets back a
//! polished illustration. (An earlier describe-then-redraw two-step was dropped —
//! the straight image-to-image redraw is more faithful to the actual drawing.)

use anyhow::{bail, Context, Result};
use base64::Engine;
use std::io::Read;

pub const DEFAULT_MODEL: &str = "google/gemini-2.5-flash-image";

/// Rough scribble -> impressive image. Turn the sketch into a polished, detailed
/// professional line illustration with a fitting background — clean confident
/// inking, NOT a messy scribble — while staying faithful to the subject and its
/// orientation. Rendered as clean linework so it reproduces well as pen strokes.
pub const STYLE_PROMPT: &str = "Transform this rough hand-drawn scribble into an impressive, polished \
black-and-white line illustration — the kind a skilled professional illustrator would ink. Use clean, \
confident, deliberate linework: crisp precise outlines and well-observed detail on the main subject, \
with tasteful fine-line shading only where it genuinely adds form (light, controlled hatching). It must \
look PROFESSIONAL and finished, never messy, scratchy or scribbly — every line purposeful. Elevate the \
rough sketch into a proper picture: add rich, believable detail to the subject and place it in a fitting, \
tasteful background and setting that suits the scene (kept lighter and less busy than the subject so the \
subject stays the clear focus), giving the whole thing depth and context. \
Stay FAITHFUL to the original: the SAME subject, the SAME composition and layout, and the SAME orientation \
and facing direction — do NOT mirror, flip, rotate or reverse anything (if it faces left it stays facing \
left, keep everything the same way up and on the same side). Refine and elevate the sketch; do not turn it \
into a different object or scene. Black ink on a clean white background, no border, full frame, no text \
unless the sketch itself contains text.";

#[derive(Clone)]
pub struct OpenRouterClient {
    api_key: String,
    model: String,
}

impl OpenRouterClient {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        Self { api_key, model: model.unwrap_or_else(|| DEFAULT_MODEL.to_string()) }
    }

    fn post(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = ureq::post("https://openrouter.ai/api/v1/chat/completions")
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(90))
            .send_string(&body.to_string());
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                // char-safe truncation (error bodies can contain multi-byte UTF-8).
                let snippet: String = text.chars().take(500).collect();
                bail!("openrouter returned {code}: {snippet}");
            }
            Err(e) => return Err(e).context("openrouter request failed"),
        };
        // Read the body via a reader with a generous cap — into_string() caps at
        // 10 MiB and a detailed full-page PNG (base64-in-JSON) can exceed that.
        let mut text = String::new();
        resp.into_reader()
            .take(64 * 1024 * 1024)
            .read_to_string(&mut text)
            .context("reading openrouter response")?;
        serde_json::from_str(&text).context("parsing openrouter response")
    }

    /// Straight redraw from the sketch (no classification step).
    pub fn sketch_to_illustration(&self, sketch_png: &[u8]) -> Result<Vec<u8>> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(sketch_png);
        let body = serde_json::json!({
            "model": self.model,
            "modalities": ["image", "text"],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}},
                    {"type": "text", "text": STYLE_PROMPT},
                ],
            }],
        });
        let v = self.post(&body)?;
        let images = v
            .pointer("/choices/0/message/images")
            .and_then(|i| i.as_array())
            .context("no images array in response")?;
        let url = images
            .first()
            .and_then(|i| i.pointer("/image_url/url"))
            .and_then(|u| u.as_str())
            .context("no image url in response")?;
        let b64_out = url.split_once(',').map(|(_, d)| d).context("image url is not a data URI")?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(b64_out)
            .context("decoding returned image base64")?;
        if let Some(cost) = v.pointer("/usage/cost").and_then(|c| c.as_f64()) {
            log::info!("openrouter generation cost: ${cost:.4}");
        }
        Ok(png)
    }
}
