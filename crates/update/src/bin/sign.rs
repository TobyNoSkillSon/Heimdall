//! heimdall-update-sign: the release key's tool, used by scripts/update_keygen.sh and scripts/package_release.sh.
//! Never prints a private key.
//!
//!   keygen <dir>                     writes <dir>/heimdall-update.key (0600, in a 0700 folder) and
//!                                    <dir>/heimdall-update.pub; prints the public key and its Rust constant
//!   sign --key <secret> <file>       writes <file>.sig (the SHA256SUMS.sig format)
//!   verify [--pub <public>] <file> <sig>   checks a signature (default: the keys compiled into this build)

use heimdall_update::signing;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |key: &str| args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let positional: Vec<&String> = {
        let mut out = vec![];
        let mut skip = false;
        for a in args.iter().skip(1) {
            if skip {
                skip = false;
            } else if a.starts_with("--") {
                skip = true;
            } else {
                out.push(a);
            }
        }
        out
    };
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let [dir] = positional.as_slice() else { return fail("usage: heimdall-update-sign keygen <dir>") };
            keygen(Path::new(dir.as_str())).unwrap_or_else(fail)
        }
        Some("sign") => {
            let (Some(key), [file]) = (option("--key"), positional.as_slice()) else {
                return fail("usage: heimdall-update-sign sign --key <secret> <file>");
            };
            let seed = match signing::read_secret(&key) {
                Ok(seed) => seed,
                Err(error) => return fail(error),
            };
            let Ok(message) = std::fs::read(file.as_str()) else { return fail(format!("cannot read {file}")) };
            let line = signing::sign(&seed, &message);
            let sig = format!("{file}.sig");
            if let Err(error) = std::fs::write(&sig, &line) {
                return fail(error);
            }
            if let Err(error) = signing::verify_sums(&message, &line, &[signing::public_key(&seed)]) {
                return fail(error);
            }
            println!("signed {file} -> {sig} (key {})", signing::key_id(&signing::public_key(&seed)));
            ExitCode::SUCCESS
        }
        Some("verify") => {
            let [file, sig] = positional.as_slice() else {
                return fail("usage: heimdall-update-sign verify [--pub <public>] <file> <sig>");
            };
            let keys: Vec<[u8; 32]> = match option("--pub") {
                Some(path) => match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| signing::decode_public(&t).map_err(|e| e.0)) {
                    Ok(key) => vec![key],
                    Err(error) => return fail(error),
                },
                None => signing::TRUSTED_KEYS.to_vec(),
            };
            let (Ok(message), Ok(text)) = (std::fs::read(file.as_str()), std::fs::read_to_string(sig.as_str())) else {
                return fail("cannot read the file or signature");
            };
            match signing::verify_sums(&message, &text, &keys) {
                Ok(()) => {
                    println!("signature ok");
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            }
        }
        _ => fail("usage: heimdall-update-sign keygen <dir> | sign --key <secret> <file> | verify [--pub <public>] <file> <sig>"),
    }
}

fn keygen(dir: &Path) -> Result<ExitCode, String> {
    let secret_path = dir.join("heimdall-update.key");
    let public_path = dir.join("heimdall-update.pub");
    if secret_path.exists() || public_path.exists() {
        return Err(format!("a key already exists in {}; nothing written", dir.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let seed = signing::generate_seed().map_err(|e| e.0)?;
    let public = signing::public_key(&seed);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&secret_path)
            .map_err(|e| e.to_string())?;
        file.write_all(signing::encode_secret(&seed).as_bytes()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::write(&public_path, signing::encode_public(&public)).map_err(|e| e.to_string())?;
    println!("secret key: {} (0600; keep it out of every repository)", secret_path.display());
    println!("public key: {}", public_path.display());
    println!("key id: {}", signing::key_id(&public));
    println!("public key (base64): {}", signing::encode_public(&public).trim());
    println!("paste into crates/update/src/signing.rs:");
    println!("{}", signing::rust_constant(&public));
    Ok(ExitCode::SUCCESS)
}
