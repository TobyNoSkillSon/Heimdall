//! Images arrive from HTTP clients: truncated or corrupted bytes must come back as an error or an image, never a
//! panic.

use splash::images::prepare;

#[test]
fn images_robust_corrupt_inputs_never_panic() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images");
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        // The 36 MP source-limit fixture is refused before decoding; mutants of it would only decode slowly.
        if path.extension().is_some_and(|e| e == "json") || bytes.len() > 200_000 {
            continue;
        }
        for cut in [bytes.len() / 3, bytes.len() / 2, bytes.len() * 9 / 10, bytes.len().saturating_sub(2)] {
            let _ = prepare(&bytes[..cut], 65_536);
        }
        for _ in 0..12 {
            let mut mutant = bytes.clone();
            for _ in 0..1 + next() % 4 {
                let at = (next() % mutant.len() as u64) as usize;
                mutant[at] = next() as u8;
            }
            let _ = prepare(&mutant, 65_536);
        }
    }
}
