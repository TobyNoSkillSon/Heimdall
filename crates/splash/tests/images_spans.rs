//! Placeholder expansion, request-size checks, content parts and the image cache, against frontend.py's rules.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::json;
use splash::images::*;

const PAD: u32 = 151_655;

fn image(grid_height: u32, grid_width: u32, seed: u8) -> Arc<PreparedImage> {
    let pixels = vec![seed; (grid_height * grid_width * PATCH * PATCH * 3) as usize];
    Arc::new(PreparedImage { grid_height, grid_width, pixels, digest_lo: seed as u64, digest_hi: !(seed as u64) })
}

fn png_data_url(width: u32, height: u32) -> String {
    let mut bytes = Vec::new();
    let img = image::RgbImage::from_fn(width, height, |x, y| image::Rgb([x as u8, y as u8, 7]));
    img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[test]
fn images_spans_expand_widens_each_template_pad() {
    // The second PAD (index 3) is quoted text, not a template placeholder: it stays one token.
    let prompt = [1, PAD, 2, PAD, 3, PAD, 4];
    let images = [image(4, 6, 1), image(2, 2, 2)];
    let out = expand_image_pads(&prompt, &images, &[1, 5], PAD, 10_000).unwrap();
    assert_eq!(image_token_count(&prompt, &images, &[1, 5], PAD).unwrap(), 7 + 5);
    let mut want = vec![1];
    want.extend([PAD; 6]);
    want.extend([2, PAD, 3, PAD, 4]);
    assert_eq!(out.tokens, want);
    assert_eq!(
        out.spans,
        [
            ImageSpan { offset: 1, tokens: 6, grid_height: 4, grid_width: 6, digest_lo: 1, digest_hi: !1 },
            ImageSpan { offset: 10, tokens: 1, grid_height: 2, grid_width: 2, digest_lo: 2, digest_hi: !2 },
        ]
    );
    assert_eq!(out.pixels.len(), images[0].pixels.len() + images[1].pixels.len());
    assert_eq!(out.pixels[images[0].pixels.len()], 2);
}

#[test]
fn images_spans_mismatched_placeholders_are_refused() {
    let images = [image(2, 2, 1)];
    let refused = Err(ImageError::Invalid("image count does not match the rendered template".into()));
    assert_eq!(expand_image_pads(&[1, 2], &images, &[1], PAD, 100), refused);
    assert_eq!(expand_image_pads(&[PAD, PAD], &images, &[0, 1], PAD, 100), refused);
    assert_eq!(expand_image_pads(&[PAD], &images, &[5], PAD, 100), refused);
}

#[test]
fn images_spans_size_checks_match_python() {
    let images = [image(32, 32, 1)]; // 256 tokens
    assert_eq!(
        expand_image_pads(&[1, PAD, 2], &images, &[1], PAD, 258),
        Err(ImageError::ContextLength { input_tokens: 258, maximum_input_tokens: 257, image_tokens_only: false })
    );
    assert!(expand_image_pads(&[1, PAD, 2], &images, &[1], PAD, 259).is_ok());
    assert_eq!(check_request_size(10, 1, 0, None, true), Ok(()));
    assert_eq!(
        check_request_size(10, 1, 0, Some(10), true),
        Err(ImageError::ContextLength { input_tokens: 10, maximum_input_tokens: 9, image_tokens_only: true })
    );
    let limit = 256 * 1024 * 1024 - 114 - 4 * 64 - 32 - 4;
    assert_eq!(check_request_size(1, 1, limit, None, false), Ok(()));
    assert_eq!(
        check_request_size(1, 1, limit + 1, None, false),
        Err(ImageError::Invalid("images exceed the request size limit".into()))
    );
}

#[test]
fn images_spans_content_parts_in_every_api_shape() {
    let url = |part| image_part_url(&part);
    assert_eq!(url(json!({"type": "image_url", "image_url": {"url": "data:x"}})), Ok(Some("data:x".into())));
    assert_eq!(url(json!({"type": "input_image", "image_url": "data:y"})), Ok(Some("data:y".into())));
    assert_eq!(
        url(json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QQ=="}})),
        Ok(Some("data:image/png;base64,QQ==".into()))
    );
    assert_eq!(url(json!({"type": "text", "text": "hi"})), Ok(None));
    assert_eq!(url(json!({"type": "image_url", "image_url": {}})), Err(ImageError::Invalid("invalid image content part".into())));
    assert_eq!(
        url(json!({"type": "image", "source": {"type": "url", "url": "https://x"}})),
        Err(ImageError::Invalid("Anthropic image blocks must be base64 sources".into()))
    );
}

#[test]
fn images_spans_prepare_images_in_render_order() {
    let cache = ImageCache::new();
    let (a, b) = (png_data_url(40, 30), png_data_url(300, 20));
    let messages = [
        json!({"role": "system", "content": "plain"}),
        json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": a}}, {"type": "text", "text": "x"}]}),
        json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": b}}, {"type": "image_url", "image_url": {"url": a}}]}),
    ];
    let batch = prepare_images(&messages, &cache, MAX_PIXELS, Some(100_000), None).unwrap();
    let grids: Vec<_> = batch.iter().map(|i| (i.grid_height, i.grid_width)).collect();
    assert_eq!(grids, [(14, 20), (6, 62), (14, 20)]);
    assert!(Arc::ptr_eq(&batch[0], &batch[2]), "a resent image is a cache hit");
    let stats = cache.stats();
    assert_eq!(stats.entries, 2);
    assert_eq!(stats.request_bytes, batch.iter().map(|i| i.pixels.len()).sum::<usize>());
    drop(batch);
    assert_eq!(cache.stats().request_bytes, 0);

    assert_eq!(
        prepare_images(&messages, &cache, MAX_PIXELS, Some(200), None).err(),
        Some(ImageError::ContextLength { input_tokens: 233, maximum_input_tokens: 199, image_tokens_only: true })
    );
    assert_eq!(cache.stats().request_bytes, 0, "a refused batch returns its charge");
    let past = Instant::now() - Duration::from_millis(1);
    assert_eq!(prepare_images(&messages, &cache, MAX_PIXELS, None, Some(past)).err(), Some(ImageError::Timeout));
    let many: Vec<_> = (0..65).map(|_| json!({"type": "image_url", "image_url": {"url": a}})).collect();
    assert_eq!(
        prepare_images(&[json!({"role": "user", "content": many})], &cache, MAX_PIXELS, None, None).err(),
        Some(ImageError::Invalid("requests support at most 64 images".into()))
    );
    let bad = [json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": "https://x/a.png"}}]})];
    assert_eq!(
        prepare_images(&bad, &cache, MAX_PIXELS, None, None).err(),
        Some(ImageError::Invalid("only data: image URLs are supported".into()))
    );
}

#[test]
fn images_spans_cache_evicts_least_recent_and_bounds_in_flight_bytes() {
    let one = 224 * 320 * 3; // a 40x30 source upscales to 14x20 patches
    let cache = ImageCache::with_budgets(2 * one, one);
    let payload = |w| decode_data_url(&png_data_url(w, 30)).unwrap();
    let (a, b, c) = (payload(40), payload(41), payload(42));
    let first = cache.prepare(&a, MAX_PIXELS).unwrap();
    cache.prepare(&b, MAX_PIXELS).unwrap();
    assert!(Arc::ptr_eq(&first, &cache.prepare(&a, MAX_PIXELS).unwrap()));
    cache.prepare(&c, MAX_PIXELS).unwrap(); // evicts b, the least recently used
    assert_eq!(cache.stats().entries, 2);
    assert!(Arc::ptr_eq(&first, &cache.prepare(&a, MAX_PIXELS).unwrap()));
    assert!(!Arc::ptr_eq(&first, &cache.prepare(&a, 65_536 * 2).unwrap()), "the budget is part of the key");

    let mut batch = cache.request_batch();
    batch.push(first.clone()).unwrap();
    assert_eq!(
        batch.push(first.clone()),
        Err(ImageError::Capacity("in-flight image memory budget is full".into()))
    );
    let mut other = cache.request_batch();
    assert!(other.push(first.clone()).is_err());
    drop(batch);
    other.push(first).unwrap();
}
