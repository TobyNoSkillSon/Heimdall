//! Signed checksums. Each release carries `SHA256SUMS` and `SHA256SUMS.sig`, an Ed25519 signature over the exact
//! bytes of `SHA256SUMS`, made by `scripts/package_release.sh` with the release key (never in the repository). The app
//! trusts only the public keys compiled in here ([`TRUSTED_KEYS`]), so a release replaced on the server without the
//! private key fails before anything is unpacked.
//!
//! `SHA256SUMS.sig`: one line, `ed25519 <key id> <base64 signature>`, where the key id is the first 16 hex digits of
//! the SHA-256 of the public key. Secret key file: `heimdall-update-ed25519-secret v1` then the base64 32-byte seed.
//! Public key file: the base64 32-byte key.

use crate::{Result, UpdateError};
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Heimdall Alpha's release key (public half), written by `scripts/update_keygen.sh`. Key id 5c612905cafd1477;
/// base64 GVezd/M2mJ0SWzq+lRMxUDbFdNPBk9SB/6OvbYnU3iw=. The private half lives outside every repository.
pub const ALPHA_KEY: [u8; 32] = [
    0x19, 0x57, 0xb3, 0x77, 0xf3, 0x36, 0x98, 0x9d, 0x12, 0x5b, 0x3a, 0xbe, 0x95, 0x13, 0x31, 0x50, 0x36, 0xc5, 0x74, 0xd3,
    0xc1, 0x93, 0xd4, 0x81, 0xff, 0xa3, 0xaf, 0x6d, 0x89, 0xd4, 0xde, 0x2c,
];

/// Keys a release signature may come from. A key rotation ships the new key here one release before it signs.
pub const TRUSTED_KEYS: &[[u8; 32]] = &[ALPHA_KEY];

const SECRET_HEADER: &str = "heimdall-update-ed25519-secret v1";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub fn key_id(public: &[u8; 32]) -> String {
    crate::hex(&Sha256::digest(public))[..16].to_owned()
}

/// Checks `sig_text` (the `.sig` file) over `sums` against `keys`.
pub fn verify_sums(sums: &[u8], sig_text: &str, keys: &[[u8; 32]]) -> Result<()> {
    let bad = || UpdateError::new("SHA256SUMS signature is not valid");
    let fields: Vec<&str> = sig_text.split_whitespace().collect();
    let [kind, id, signature] = fields.as_slice() else { return Err(bad()) };
    if *kind != "ed25519" {
        return Err(bad());
    }
    let key = keys
        .iter()
        .find(|k| key_id(k) == *id)
        .ok_or_else(|| UpdateError::new("SHA256SUMS is signed by a key this app does not trust"))?;
    let bytes = B64.decode(signature).map_err(|_| bad())?;
    let signature = Signature::from_slice(&bytes).map_err(|_| bad())?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| bad())?;
    key.verify_strict(sums, &signature).map_err(|_| bad())
}

/// The `.sig` line for `message`.
pub fn sign(seed: &[u8; 32], message: &[u8]) -> String {
    let key = SigningKey::from_bytes(seed);
    let signature = key.sign(message);
    format!("ed25519 {} {}\n", key_id(&key.verifying_key().to_bytes()), B64.encode(signature.to_bytes()))
}

pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// A new random seed from the system's generator.
pub fn generate_seed() -> Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| UpdateError::new(format!("no random bytes: {e}")))?;
    Ok(seed)
}

pub fn encode_secret(seed: &[u8; 32]) -> String {
    format!("{SECRET_HEADER}\n{}\n", B64.encode(seed))
}

pub fn encode_public(public: &[u8; 32]) -> String {
    format!("{}\n", B64.encode(public))
}

pub fn decode_public(text: &str) -> Result<[u8; 32]> {
    let bytes = B64.decode(text.trim()).map_err(|_| UpdateError::new("public key is not base64"))?;
    bytes.try_into().map_err(|_| UpdateError::new("public key is not 32 bytes"))
}

/// Reads a secret key file; refuses one that group or others can read.
pub fn read_secret(path: &Path) -> Result<[u8; 32]> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|_| UpdateError::new(format!("no signing key at {}", path.display())))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(UpdateError::new(format!("signing key {} must be private (chmod 600)", path.display())));
    }
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    if lines.next() != Some(SECRET_HEADER) {
        return Err(UpdateError::new("not a Heimdall update signing key"));
    }
    let bytes = B64.decode(lines.next().unwrap_or_default().trim()).map_err(|_| UpdateError::new("bad key"))?;
    bytes.try_into().map_err(|_| UpdateError::new("bad key length"))
}

/// The Rust constant for a public key, as `update_keygen.sh` prints it.
pub fn rust_constant(public: &[u8; 32]) -> String {
    let bytes: Vec<String> = public.iter().map(|b| format!("0x{b:02x}")).collect();
    format!("pub const ALPHA_KEY: [u8; 32] = [{}];", bytes.join(", "))
}
