use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
#[derive(serde::Deserialize)]
pub(crate) struct Asset {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}
pub fn verify(path: &Path, bytes: u64, expected: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() == bytes,
        "model size does not match pinned artifact: {}",
        path.display()
    );
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == expected,
        "model hash does not match pinned artifact: {}",
        path.display()
    );
    Ok(())
}
