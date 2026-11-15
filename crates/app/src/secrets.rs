//! Night 1111 [input]: secrets the user hands Heimdall through the composer's lock (bug note 2026-11-10T16-30-03: "there
//! is no safe way to hand Heimdall a secret; pasting a key into the conversation stores it in the transcript").
//!
//! The island's secure field sends the value as `ui::UiAction::Secret` (its `Debug` prints `<secret>`). It is written
//! here, and only here: one file per secret under `<data dir>/secrets/` (the folder 0700, the file 0600, created with
//! those modes, never wider for a moment). The agent gets a typed message with the file's path, never the value, so
//! the transcript, the archive, the logs and the conversation hold only the path. A task reads the value from that file
//! inside the command that needs it (`KEY="$(cat <path>)" tool …`).

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The folder under Heimdall's data directory that holds the secrets.
pub const DIR: &str = "secrets";

/// How long a secret's file is kept (review fix S3, night 1111 fresh review: "each secret stays forever").
pub const KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Deletes every secret file in `data/secrets/` last written more than `KEEP` before `now`; called at launch and before
/// each new secret is kept. Only `secret-*.txt` files are touched. Returns how many went.
pub fn sweep(data: &Path, now: std::time::SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(data.join(DIR)) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("secret-") && name.ends_with(".txt")) {
            continue;
        }
        let old = e.metadata().ok().filter(|m| m.is_file()).and_then(|m| m.modified().ok()).and_then(|t| now.duration_since(t).ok()).is_some_and(|age| age > KEEP);
        if old && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Keeps `secret` as a new 0600 file under `data/secrets/`, named for the moment it came (`local` is the wall time,
/// `YYYYMMDD-HHMMSS`); a second secret in the same second gets `-2`, `-3`. Returns the file's path.
pub fn keep(data: &Path, secret: &ui::SecretText, local: &str) -> Result<PathBuf> {
    sweep(data, std::time::SystemTime::now());
    let dir = data.join(DIR);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // A folder made before (or by hand) is narrowed to the user alone.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    for n in 1..1000 {
        let name = if n == 1 { format!("secret-{local}.txt") } else { format!("secret-{local}-{n}.txt") };
        let path = dir.join(name);
        let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path);
        match file {
            Ok(mut f) => {
                f.write_all(secret.expose().trim().as_bytes())?;
                f.sync_all()?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    anyhow::bail!("no free secret name for {local}")
}

/// The typed message the agent gets in the secret's place: where it is and how to use it, never the value.
///
/// Night 1112 [secret]: the example command quotes the path. The data directory is `~/Library/Application Support/…`,
/// so the unquoted `$(cat <path>)` it used to give split at the space and read nothing (lane K check, 12 Nov).
pub fn reference(path: &Path) -> String {
    format!(
        "I put a secret in the secret field. It is saved at {} (only I can read it), kept for 24 hours and then deleted. Use it only by reading that file inside the command that needs it, for example KEY=\"$(cat {})\", and hand it to a tool on stdin or in an environment variable rather than as a command-line argument; never print, log, echo or repeat its value, and never put it in a message, file or transcript.",
        path.display(),
        shell_quote(&path.display().to_string())
    )
}

/// `s` as one single-quoted POSIX shell word.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Wall time now as `YYYYMMDD-HHMMSS` (local), for the secret's name.
pub fn now_stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heimdall-secrets-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_secret_is_kept_owner_only_and_the_message_never_holds_it() {
        let data = lab_dir("keep");
        let value = "sk-lab-0123456789abcdef";
        let path = keep(&data, &ui::SecretText::new(format!("  {value}\n")), "20261110-213000").unwrap();
        assert_eq!(path, data.join("secrets/secret-20261110-213000.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), value, "kept trimmed, byte for byte");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(data.join("secrets")).unwrap().permissions().mode() & 0o777, 0o700);
        let message = reference(&path);
        assert!(!message.contains(value), "the agent's message never carries the value");
        assert!(message.contains(&path.display().to_string()));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn two_secrets_in_one_second_never_overwrite_each_other() {
        let data = lab_dir("twice");
        let a = keep(&data, &ui::SecretText::new("first"), "20261110-213000").unwrap();
        let b = keep(&data, &ui::SecretText::new("second"), "20261110-213000").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "second");
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// Night 1112 [secret] (lane K): the example command in the agent's message works as written in a real shell,
    /// with the data directory's space ("Application Support") and a quote in the path, and reads the value whole.
    #[test]
    fn the_messages_example_command_reads_the_secret_in_a_real_shell() {
        let root = lab_dir("shell");
        let data = root.join("Application Support/Heim'dall");
        let value = "abcd efgh ijkl mnop";
        let path = keep(&data, &ui::SecretText::new(value), "20261111-111753").unwrap();
        let message = reference(&path);
        let example = message.split("for example ").nth(1).and_then(|r| r.split(", and hand it").next()).expect("an example command");
        assert!(!example.contains(value));
        let script = format!("{example}; [ \"$KEY\" = '{value}' ] && echo same");
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&script).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "same", "the example reads the file: {}", String::from_utf8_lossy(&out.stderr));
        assert!(message.contains("stdin"), "the message says how to hand the value on without argv");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Review fix S3 (night 1111 fresh review): secrets older than 24 hours are deleted at launch and on each new one,
    /// and the agent's message says how long it is kept.
    #[test]
    fn secrets_older_than_a_day_are_deleted_and_the_message_says_so() {
        let data = lab_dir("sweep");
        let old = keep(&data, &ui::SecretText::new("old"), "20261108-090000").unwrap();
        let fresh = keep(&data, &ui::SecretText::new("fresh"), "20261110-090000").unwrap();
        let other = data.join(DIR).join("notes.txt");
        std::fs::write(&other, "not a secret file").unwrap();
        let day_ago = std::time::SystemTime::now() - KEEP - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(day_ago).unwrap();
        std::fs::File::options().write(true).open(&other).unwrap().set_modified(day_ago).unwrap();
        assert_eq!(sweep(&data, std::time::SystemTime::now()), 1);
        assert!(!old.exists() && fresh.exists() && other.exists(), "only the old secret goes");
        // Keeping a new secret sweeps too.
        std::fs::File::options().write(true).open(&fresh).unwrap().set_modified(day_ago).unwrap();
        let newest = keep(&data, &ui::SecretText::new("newest"), "20261110-220000").unwrap();
        assert!(!fresh.exists() && newest.exists());
        assert!(reference(&newest).contains("kept for 24 hours"));
        assert_eq!(sweep(&data.join("missing"), std::time::SystemTime::now()), 0);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
