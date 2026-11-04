//! Image input for vision models: Splash 1.4.1's `server/images.py`, the image steps of `frontend.py`
//! (`_prepare_images`, `_check_image_request_size`, `_image_token_count`, `_expand_image_pads`) and the image parts
//! of `api_shapes.py`.
//!
//! Prepared pixels are byte-identical to Python's (Pillow 13 with libjpeg-turbo's NEON IDCT, as on Apple silicon)
//! for PNG, GIF, WebP and JPEG, orientation included: the engine's prefix cache keys on a digest of those bytes, so a
//! Rust server and the Python one share cache identity, and the model sees the same input. `tests/images_golden.rs`
//! holds that against Splash's own output. Where Python decodes and this refuses: arithmetic-coded and lossless
//! JPEGs, progressive JPEGs whose scans stop early, and non-integral chroma subsampling (each a clear 400). Damaged
//! files diverge in places: Pillow's partial images after a decoder error, and corrupt deflate or lossy WebP data.

mod cache;
mod decode;
mod gif;
mod jpeg;
mod meta;
mod resample;
mod spans;

pub use cache::{ImageCache, ImageCacheStats, PreparedImages};
pub use resample::resize_bicubic_rgb;
pub use spans::{check_request_size, expand_image_pads, image_token_count, prepare_images, ExpandedPrompt, ImageSpan};

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Vision patch side in pixels.
pub const PATCH: u32 = 16;
/// Patches merged per language token along each side.
pub const MERGE: u32 = 2;
/// Resized sides are whole merge units.
pub const FACTOR: u32 = PATCH * MERGE;
/// Qwen3-VL's shortest-edge floor.
pub const MIN_PIXELS: u32 = 65_536;
/// Serving default and largest cap: 16,384 patches, the protocol's per-image ceiling.
pub const MAX_PIXELS: u32 = 4_194_304;
/// Bounds codec work before a full-resolution decode; 32 MP photos fit.
pub const MAX_SOURCE_PIXELS: u64 = 32 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Why an image was refused. `Invalid` answers 400; `Capacity` answers 503 `frontend_overloaded`; `ContextLength`
/// answers 400 `context_length_exceeded`; `Timeout` answers 504 `request_timeout`. Messages are Python's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Capacity(String),
    #[error("prompt exceeds the context window")]
    ContextLength { input_tokens: usize, maximum_input_tokens: usize, image_tokens_only: bool },
    #[error("request timed out")]
    Timeout,
}

fn invalid(message: &str) -> ImageError {
    ImageError::Invalid(message.to_owned())
}

/// One image resized into its patch grid: packed RGB, `grid_height * PATCH` rows of `grid_width * PATCH` pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    pub grid_height: u32,
    pub grid_width: u32,
    pub pixels: Vec<u8>,
    /// SHA-256 of `<u32 grid_height><u32 grid_width>` (little-endian) and the pixels: its first and second u64.
    pub digest_lo: u64,
    pub digest_hi: u64,
}

impl PreparedImage {
    /// Merged language tokens the image occupies.
    pub fn tokens(&self) -> u32 {
        (self.grid_height / MERGE) * (self.grid_width / MERGE)
    }
}

/// Python's `base64.b64decode(validate=True)`: standard alphabet, canonical padding, no whitespace, and (unlike the
/// `base64` crate's default) non-zero trailing bits allowed.
const PY_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// The bytes of a `data:` URL. Only base64 data URLs are accepted; http(s) URLs are never fetched.
pub fn decode_data_url(url: &str) -> Result<Vec<u8>, ImageError> {
    if !url.starts_with("data:") {
        return Err(invalid("only data: image URLs are supported"));
    }
    let (header, encoded) = url.split_once(',').unwrap_or((url, ""));
    if !header.contains(";base64") {
        return Err(invalid("image data URLs must be base64-encoded"));
    }
    if encoded.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
        return Err(invalid("image exceeds the size limit"));
    }
    let raw = PY_BASE64.decode(encoded).map_err(|_| invalid("image data URL is not valid base64"))?;
    if raw.len() > MAX_IMAGE_BYTES {
        return Err(invalid("image exceeds the size limit"));
    }
    Ok(raw)
}

/// The URL of an image content part in any API shape Python accepts, or `None` for a part that is not an image:
/// OpenAI chat `image_url` and Responses `input_image` (`image_url` as a string or `{url}`), and Anthropic `image`
/// blocks with a base64 source, which become a data URL. The URL is checked when decoded, not here.
pub fn image_part_url(part: &Value) -> Result<Option<String>, ImageError> {
    match part.get("type").and_then(Value::as_str) {
        Some("image_url" | "input_image") => {
            let image_url = part.get("image_url");
            let url = match image_url {
                Some(Value::Object(object)) => object.get("url"),
                other => other,
            };
            match url {
                Some(Value::String(url)) => Ok(Some(url.clone())),
                _ => Err(invalid("invalid image content part")),
            }
        }
        Some("image") => {
            let source = part.get("source");
            let field = |key| source.and_then(|s| s.get(key)).and_then(Value::as_str);
            match (field("type"), field("media_type"), field("data")) {
                (Some("base64"), Some(media_type), Some(data)) => {
                    Ok(Some(format!("data:{media_type};base64,{data}")))
                }
                _ => Err(invalid("Anthropic image blocks must be base64 sources")),
            }
        }
        _ => Ok(None),
    }
}

/// The most patches an image resized within `max_pixels` can have: its sides are whole merge units, so its patch
/// count is a multiple of 4.
pub fn max_patches(max_pixels: u32) -> u32 {
    max_pixels / (PATCH * PATCH) / 4 * 4
}

/// Python's `round`: half to even.
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 { 2.0 * (x / 2.0).round() } else { r }
}

/// Upstream Qwen2-VL smart resize: `(height, width)` aligned to `FACTOR`, inside the pixel budget, aspect ratio
/// kept as closely as possible.
pub fn smart_resize(height: u32, width: u32, max_pixels: u32) -> Result<(u32, u32), ImageError> {
    if height == 0 || width == 0 {
        return Err(invalid("image dimensions must be positive"));
    }
    if !(MIN_PIXELS..=MAX_PIXELS).contains(&max_pixels) {
        return Err(invalid("invalid image pixel budget"));
    }
    let (h, w, f) = (height as f64, width as f64, FACTOR as f64);
    if h.max(w) / h.min(w) > 200.0 {
        return Err(invalid("absolute aspect ratio must be smaller than 200"));
    }
    let mut h_bar = round_half_even(h / f) as i64 * FACTOR as i64;
    let mut w_bar = round_half_even(w / f) as i64 * FACTOR as i64;
    let budget = max_pixels as i64;
    if h_bar * w_bar > budget {
        let beta = ((h * w) / max_pixels as f64).sqrt();
        h_bar = ((h / beta / f).floor() as i64 * FACTOR as i64).max(FACTOR as i64);
        w_bar = ((w / beta / f).floor() as i64 * FACTOR as i64).max(FACTOR as i64);
    } else if h_bar * w_bar < MIN_PIXELS as i64 {
        let beta = (MIN_PIXELS as f64 / (h * w)).sqrt();
        h_bar = (h * beta / f).ceil() as i64 * FACTOR as i64;
        w_bar = (w * beta / f).ceil() as i64 * FACTOR as i64;
    }
    // Upscaling to the minimum can overshoot a small cap after alignment; the hard cap wins over the soft minimum.
    while h_bar * w_bar > budget {
        if h_bar >= w_bar && h_bar > FACTOR as i64 {
            h_bar -= FACTOR as i64;
        } else {
            w_bar -= FACTOR as i64;
        }
    }
    Ok((h_bar as u32, w_bar as u32))
}

/// Decodes, turns upright, flattens onto white, resizes into the grid, and digests the pixels.
pub fn prepare(payload: &[u8], max_pixels: u32) -> Result<PreparedImage, ImageError> {
    let source = decode::open(payload)?;
    let stored = source.dimensions();
    if stored.0 as u64 * stored.1 as u64 > MAX_SOURCE_PIXELS {
        return Err(invalid("source image exceeds the pixel limit"));
    }
    // Geometry comes from the stored size, before the EXIF turn, as in Python.
    let (mut height, mut width) = smart_resize(stored.1, stored.0, max_pixels)?;
    let rgb = source.decode_rgb()?;
    if (rgb.width, rgb.height) != stored {
        (height, width) = (width, height);
    }
    let pixels = resize_bicubic_rgb(
        &rgb.pixels,
        rgb.width as usize,
        rgb.height as usize,
        width as usize,
        height as usize,
    );
    let (grid_height, grid_width) = (height / PATCH, width / PATCH);
    let mut hasher = Sha256::new();
    hasher.update(grid_height.to_le_bytes());
    hasher.update(grid_width.to_le_bytes());
    hasher.update(&pixels);
    let digest = hasher.finalize();
    Ok(PreparedImage {
        grid_height,
        grid_width,
        pixels,
        digest_lo: u64::from_le_bytes(digest[..8].try_into().unwrap()),
        digest_hi: u64::from_le_bytes(digest[8..16].try_into().unwrap()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_rounding_is_half_to_even() {
        assert_eq!([0.5, 1.5, 2.5, -0.5, 2.4, 2.6].map(round_half_even), [0.0, 2.0, 2.0, -0.0, 2.0, 3.0]);
    }

    #[test]
    fn max_patches_is_a_multiple_of_four() {
        assert_eq!(max_patches(MAX_PIXELS), 16_384);
        assert_eq!(max_patches(100_000), 388);
    }
}
