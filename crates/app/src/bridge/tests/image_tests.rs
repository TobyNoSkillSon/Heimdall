//! Screenshots entering the agent's context: fitted to the context edge, while the archive keeps the capture.
use super::*;

fn capture(width: u32, height: u32, noisy: bool) -> Vec<u8> {
    let mut image = image::RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let v = if noisy { ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 3) as u8 } else { 200 };
        *pixel = image::Rgb([v, v.wrapping_add(x as u8), (y % 251) as u8]);
    }
    let mut bytes = vec![];
    image.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    bytes
}

fn decoded_size(bytes: &[u8]) -> (u32, u32) {
    let decoded = image::load_from_memory(bytes).unwrap();
    (decoded.width(), decoded.height())
}

#[test]
fn a_3456_by_2234_capture_enters_context_at_2000_px_on_its_long_edge() {
    let original = capture(3456, 2234, false);
    let attachment = context_attachment(original.clone()).unwrap();
    assert_eq!(attachment.media_type, "image/png");
    assert_eq!(decoded_size(&attachment.bytes), (2000, 1293), "fitted, aspect ratio kept");
    assert!(attachment.bytes.len() < original.len() / 2, "{} bytes, original {}", attachment.bytes.len(), original.len());
}

#[test]
fn a_noisy_capture_too_large_for_png_goes_as_jpeg_at_the_same_edge() {
    let attachment = context_attachment(capture(3456, 2234, true)).unwrap();
    assert_eq!(attachment.media_type, "image/jpeg");
    assert_eq!(decoded_size(&attachment.bytes), (2000, 1293));
}

/// 20 Oct 19:20:52: `look` on the YouTube home page went as JPEG but was labelled PNG, and the provider refused
/// every request after it. The label is the bytes' own, for both formats.
#[test]
fn look_labels_its_screenshot_with_the_type_its_bytes_are() {
    use base64::Engine;
    for (capture, expected) in [(capture(3456, 2234, true), "image/jpeg"), (capture(800, 600, false), "image/png")] {
        let block = context_attachment(capture).unwrap().block().unwrap();
        let result = crate::drawing::look_result(&block, "Display 1: size 1728 1117");
        let data = base64::engine::general_purpose::STANDARD.decode(result["content"][0]["data"].as_str().unwrap()).unwrap();
        assert_eq!(result["content"][0]["mimeType"], expected);
        assert_eq!(agent::history::sniff(&data), Some(expected));
        assert_eq!(result["content"][1]["text"], "Display 1: size 1728 1117");
    }
}

#[test]
fn a_capture_within_both_limits_passes_through_unchanged() {
    let small = capture(800, 600, false);
    let attachment = context_attachment(small.clone()).unwrap();
    assert_eq!(attachment.media_type, "image/png");
    assert_eq!(attachment.bytes, small);
}

#[test]
fn the_archive_keeps_the_full_capture_and_the_agent_gets_the_fitted_one() {
    let d = tempfile::tempdir().unwrap();
    let journal = heimdall_archive::Journal::open(&d.path().join("session")).unwrap();
    let original = capture(3456, 2234, false);
    let shot_path = d.path().join("shot.png");
    std::fs::write(&shot_path, &original).unwrap();
    let action = screen::Action {
        source: "agent".into(),
        trigger_ns: 1,
        note: "screenshot attached".into(),
        image_path: shot_path.clone(),
        shots: vec![screen::Shot {
            display_id: 1,
            frame_ns: 1,
            kind: screen::ShotKind::FullResolution,
            width: 3456,
            height: 2234,
            grab_latency_ns: None,
            path: shot_path,
        }],
        full_resolution_errors: vec![],
        full_resolution_elapsed_ns: None,
    };
    let (images, captured) = archive_screenshot_action(&journal, &action, "agent", &[], 1).unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(decoded_size(&images[0].bytes), (2000, 1293));
    let archived_id = captured[0]["image_id"].as_str().unwrap();
    assert_eq!(journal.image_bytes(archived_id).unwrap().1, original, "the archive keeps the capture at full size");
}

/// Bug note 2026-10-25T21-31-53: the agent gets the frame from when the user said "look at the screen"; a full-resolution
/// grab replaces it only when taken at about that moment, and a late one comes beside it, second.
#[test]
fn a_late_full_resolution_frame_is_attached_beside_the_phrase_time_frame() {
    let d = tempfile::tempdir().unwrap();
    let journal = heimdall_archive::Journal::open(&d.path().join("session")).unwrap();
    let trigger = 10_000_000_000u64;
    for (grabbed, expected) in [(trigger + 200_000_000, vec![screen::ShotKind::FullResolution]),
        (trigger + 4_000_000_000, vec![screen::ShotKind::Ring, screen::ShotKind::FullResolution])] {
        let shot = |kind, frame_ns, name: &str, side: u32| {
            let path = d.path().join(name);
            std::fs::write(&path, capture(side, side, false)).unwrap();
            screen::Shot { display_id: 1, frame_ns, kind, width: side, height: side, grab_latency_ns: None, path }
        };
        let shots = vec![shot(screen::ShotKind::Ring, trigger - 1_000_000, "ring.png", 64), shot(screen::ShotKind::FullResolution, grabbed, "full.png", 96)];
        let action = screen::Action { source: "mic".into(), trigger_ns: trigger, note: "[screenshot attached]".into(),
            image_path: shots[0].path.clone(), shots, full_resolution_errors: vec![], full_resolution_elapsed_ns: None };
        let (images, _) = archive_screenshot_action(&journal, &action, "mic", &[], trigger).unwrap();
        let kinds: Vec<_> = images.iter().map(|i| if decoded_size(&i.bytes).0 == 64 { screen::ShotKind::Ring } else { screen::ShotKind::FullResolution }).collect();
        assert_eq!(kinds, expected, "grabbed {} ms after the trigger", (grabbed - trigger) / 1_000_000);
    }
}
