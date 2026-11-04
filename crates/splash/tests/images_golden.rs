//! Splash 1.4.1's own Python output (`lab/images_golden.py`, Pillow 13) against the Rust port, byte for byte.

use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};
use splash::images::{decode_data_url, prepare, smart_resize, ImageError};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images")
}

fn golden() -> Value {
    serde_json::from_slice(&std::fs::read(fixtures().join("golden.json")).unwrap()).unwrap()
}

fn message(error: ImageError) -> String {
    error.to_string()
}

#[test]
#[ignore = "needs the JPEG and .bin image fixtures that are not in the public repository"]
fn images_golden_prepare_matches_python() {
    let golden = golden();
    let mut failures = Vec::new();
    for case in golden["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let payload = std::fs::read(fixtures().join(case["file"].as_str().unwrap())).unwrap();
        let max_pixels = case["max_pixels"].as_u64().unwrap() as u32;
        let got = prepare(&payload, max_pixels);
        // Inputs libjpeg-turbo decodes but the port refuses on purpose carry the refusal instead.
        let want_error = case.get("rust_error").or(case.get("error")).and_then(Value::as_str);
        let verdict = match (want_error, got) {
            (Some(want), Err(error)) if message(error.clone()) == want => continue,
            (Some(want), got) => format!("want error {want:?}, got {:?}", got.map(|p| (p.grid_height, p.grid_width))),
            (None, Err(error)) => format!("unexpected error {error}"),
            (None, Ok(p)) => {
                let grid = (p.grid_height as u64, p.grid_width as u64);
                let want_grid = (case["grid_height"].as_u64().unwrap(), case["grid_width"].as_u64().unwrap());
                let sha = hex::encode(Sha256::digest(&p.pixels));
                let digest = (p.digest_lo.to_string(), p.digest_hi.to_string());
                let want_digest = (case["digest_lo"].as_str().unwrap().to_owned(), case["digest_hi"].as_str().unwrap().to_owned());
                if grid != want_grid {
                    format!("grid {grid:?} != {want_grid:?}")
                } else if sha != case["pixels_sha256"].as_str().unwrap() {
                    "pixels differ".to_owned()
                } else if digest != want_digest {
                    "digest differs".to_owned()
                } else if p.tokens() as u64 != case["tokens"].as_u64().unwrap() {
                    "tokens differ".to_owned()
                } else {
                    continue;
                }
            }
        };
        failures.push(format!("{name}: {verdict}"));
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn images_golden_smart_resize_matches_python() {
    for row in golden()["smart_resize"].as_array().unwrap() {
        let [h, w, budget] = [0, 1, 2].map(|i| row[i].as_u64().unwrap() as u32);
        let got = smart_resize(h, w, budget).map_err(message);
        let want = match &row[3] {
            Value::String(error) => Err(error.clone()),
            v => Ok((v.as_u64().unwrap() as u32, row[4].as_u64().unwrap() as u32)),
        };
        assert_eq!(got, want, "smart_resize({h}, {w}, {budget})");
    }
}

#[test]
fn images_golden_data_urls_match_python() {
    for row in golden()["data_urls"].as_array().unwrap() {
        let url = row[0].as_str().unwrap();
        let got = decode_data_url(url).map(hex::encode).map_err(message);
        let want = match &row[1] {
            Value::String(hex) => Ok(hex.clone()),
            v => Err(v["error"].as_str().unwrap().to_owned()),
        };
        assert_eq!(got, want, "{url}");
    }
}
