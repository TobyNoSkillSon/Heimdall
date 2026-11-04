//! From chat messages to the engine's image spans: `frontend.py`'s `_prepare_images`, `_check_image_request_size`,
//! `_image_token_count` and `_expand_image_pads`.
//!
//! The chat template renders one pad token per image; the chat layer finds which token positions the template
//! itself emitted (not pad text quoted in a message) and passes them here. Each is widened to the image's merged
//! token count, and its span tells the engine where to inject the vision embeddings.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::{decode_data_url, image_part_url, invalid, ImageCache, ImageError, PreparedImage, PreparedImages};

// Wire limits from Splash's protocol.py (version 11). Duplicated until `protocol.rs` exports them; the lead may
// point these at it.
const MAX_IMAGE_SPANS: usize = 64;
const REQUEST_FIXED_BYTES: usize = 114;
const IMAGE_SPAN_BYTES: usize = 32;
const MAX_THINKING_END_TOKENS: usize = 64;
const MAX_FRAME_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

/// One image in the prompt, laid out as protocol.py's `<IIIIQQ>` `ImageSpan`: its pad-token run, the patch grid and
/// the 128-bit content digest the engine's cache keys on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSpan {
    pub offset: u32,
    pub tokens: u32,
    pub grid_height: u32,
    pub grid_width: u32,
    pub digest_lo: u64,
    pub digest_hi: u64,
}

/// A prompt with every image placeholder widened, ready for a REQUEST frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedPrompt {
    pub tokens: Vec<u32>,
    pub spans: Vec<ImageSpan>,
    /// Every image's pixels, concatenated in span order.
    pub pixels: Vec<u8>,
}

/// Prepares every `image_url` part of canonical chat messages, in message then part order (the template's render
/// order). `max_context` is `None` where Python passes `check_context=False`; `deadline` is checked before each
/// decode, since one can take a few hundred milliseconds.
pub fn prepare_images(
    messages: &[Value],
    cache: &ImageCache,
    max_pixels: u32,
    max_context: Option<usize>,
    deadline: Option<Instant>,
) -> Result<PreparedImages, ImageError> {
    let parts: Vec<&Value> = messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
        .collect();
    if parts.len() > MAX_IMAGE_SPANS {
        return Err(ImageError::Invalid(format!("requests support at most {MAX_IMAGE_SPANS} images")));
    }
    let mut prepared = cache.request_batch();
    let (mut tokens, mut pixel_bytes) = (0, 0);
    for part in parts {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(ImageError::Timeout);
        }
        let url = image_part_url(part)?.ok_or_else(|| invalid("invalid image content part"))?;
        let image = cache.prepare(&decode_data_url(&url)?, max_pixels)?;
        tokens += image.tokens() as usize;
        pixel_bytes += image.pixels.len();
        check_request_size(tokens, prepared.len() + 1, pixel_bytes, max_context, true)?;
        prepared.push(image)?;
    }
    Ok(prepared)
}

/// Refuses a request whose tokens reach the context (when `max_context` is given) or whose REQUEST frame, with
/// room for a thinking budget's end tokens, would exceed the protocol's payload limit.
pub fn check_request_size(
    tokens: usize,
    image_count: usize,
    pixel_bytes: usize,
    max_context: Option<usize>,
    image_tokens_only: bool,
) -> Result<(), ImageError> {
    if let Some(max_context) = max_context {
        if tokens >= max_context {
            return Err(ImageError::ContextLength {
                input_tokens: tokens,
                maximum_input_tokens: max_context.saturating_sub(1),
                image_tokens_only,
            });
        }
    }
    let frame_bytes = REQUEST_FIXED_BYTES
        + 4 * tokens
        + IMAGE_SPAN_BYTES * image_count
        + pixel_bytes
        + 4 * MAX_THINKING_END_TOKENS;
    if frame_bytes > MAX_FRAME_PAYLOAD_BYTES {
        return Err(invalid("images exceed the request size limit"));
    }
    Ok(())
}

/// The prompt's length once every placeholder is widened. Fails unless each position holds `pad_id` and there is
/// exactly one position per image.
pub fn image_token_count(
    prompt: &[u32],
    images: &[Arc<PreparedImage>],
    positions: &[usize],
    pad_id: u32,
) -> Result<usize, ImageError> {
    if positions.len() != images.len() || positions.iter().any(|&p| prompt.get(p) != Some(&pad_id)) {
        return Err(invalid("image count does not match the rendered template"));
    }
    Ok(prompt.len() + images.iter().map(|i| i.tokens() as usize - 1).sum::<usize>())
}

/// Widens the template's single pad token per image to `image.tokens()` pads and returns the spans the engine
/// injects into. Sizes are checked before anything is copied: HTTP body and cache limits do not bound the decoded
/// request.
pub fn expand_image_pads(
    prompt: &[u32],
    images: &[Arc<PreparedImage>],
    positions: &[usize],
    pad_id: u32,
    max_context: usize,
) -> Result<ExpandedPrompt, ImageError> {
    let token_count = image_token_count(prompt, images, positions, pad_id)?;
    let pixel_bytes = images.iter().map(|i| i.pixels.len()).sum();
    check_request_size(token_count, images.len(), pixel_bytes, Some(max_context), false)?;
    let mut tokens = Vec::with_capacity(token_count);
    let mut spans = Vec::with_capacity(images.len());
    let mut pixels = Vec::with_capacity(pixel_bytes);
    let mut cursor = 0;
    for (&position, image) in positions.iter().zip(images) {
        tokens.extend_from_slice(prompt.get(cursor..position).unwrap_or_default());
        spans.push(ImageSpan {
            offset: tokens.len() as u32,
            tokens: image.tokens(),
            grid_height: image.grid_height,
            grid_width: image.grid_width,
            digest_lo: image.digest_lo,
            digest_hi: image.digest_hi,
        });
        tokens.extend(std::iter::repeat_n(pad_id, image.tokens() as usize));
        pixels.extend_from_slice(&image.pixels);
        cursor = position + 1;
    }
    tokens.extend_from_slice(&prompt[cursor.min(prompt.len())..]);
    Ok(ExpandedPrompt { tokens, spans, pixels })
}
