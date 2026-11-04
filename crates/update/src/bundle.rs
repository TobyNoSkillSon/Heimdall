//! Checks on an app bundle: Info.plist identity and version, required executables, code signature and designated
//! requirement (through `/usr/bin/codesign`), links that leave the bundle, and the quarantine attribute.

use crate::{Product, Result, UpdateError, Version};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub bundle_id: String,
    pub version: String,
    pub build: String,
}

pub fn info(app: &Path) -> Result<Info> {
    let path = app.join("Contents/Info.plist");
    let value = plist::Value::from_file(&path)
        .map_err(|_| UpdateError::new(format!("cannot read {}", path.display())))?;
    let dict = value.as_dictionary().ok_or_else(|| UpdateError::new("Info.plist is not a dictionary"))?;
    let text = |key: &str| dict.get(key).and_then(plist::Value::as_string).unwrap_or_default().to_owned();
    Ok(Info { bundle_id: text("CFBundleIdentifier"), version: text("CFBundleShortVersionString"), build: text("CFBundleVersion") })
}

/// The bundle is the product at `version`: its ID, its version string equal to the release's, every required
/// executable present.
pub fn check_bundle(app: &Path, version: &Version, product: &Product) -> Result<()> {
    for file in product.required {
        let path = app.join("Contents").join(file);
        let executable = std::fs::symlink_metadata(&path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !executable {
            return Err(UpdateError::new(format!("release archive lacks Contents/{file}")));
        }
    }
    let info = info(app)?;
    if info.bundle_id != product.bundle_id {
        return Err(UpdateError::new(format!("release archive does not hold {} ({})", product.app_folder, product.bundle_id)));
    }
    if Version::parse(&info.version).map(|v| v.to_string()) != Some(version.to_string()) {
        return Err(UpdateError::new("version in app does not match the release"));
    }
    Ok(())
}

/// No symlink inside the bundle points outside it.
pub fn reject_external_links(app: &Path) -> Result<()> {
    let root = app.canonicalize()?;
    for path in walk(app) {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let resolved = path.canonicalize().map_err(|_| UpdateError::new(format!("broken link in bundle: {}", path.display())))?;
            if !resolved.starts_with(&root) {
                return Err(UpdateError::new(format!("bundle contains an external link: {}", path.display())));
            }
        }
    }
    Ok(())
}

/// Every path under `root`, root first; links are listed, not followed.
pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    let mut i = 0;
    while i < out.len() {
        let path = out[i].clone();
        i += 1;
        let is_dir = std::fs::symlink_metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            if let Ok(entries) = std::fs::read_dir(&path) {
                let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                children.sort();
                out.extend(children);
            }
        }
    }
    out
}

const QUARANTINE: &str = "com.apple.quarantine";

/// Removes com.apple.quarantine from every file of `root`. Heimdall Alpha is signed with its own certificate and not
/// notarised: a quarantined copy would be blocked by Gatekeeper at its first launch.
pub fn remove_quarantine(root: &Path) -> Result<()> {
    let name = CString::new(QUARANTINE).unwrap();
    for path in walk(root) {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| UpdateError::new("path with a NUL"))?;
        // SAFETY: both strings are valid NUL-terminated C strings for the duration of the calls.
        unsafe {
            if libc::getxattr(c.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, libc::XATTR_NOFOLLOW) >= 0
                && libc::removexattr(c.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) != 0
            {
                return Err(UpdateError::new(format!("could not clear quarantine on {}", path.display())));
            }
        }
    }
    Ok(())
}

pub fn is_quarantined(root: &Path) -> bool {
    let name = CString::new(QUARANTINE).unwrap();
    walk(root).iter().any(|path| {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
        // SAFETY: valid C strings; a null buffer with size 0 only asks for the attribute's size.
        unsafe { libc::getxattr(c.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, libc::XATTR_NOFOLLOW) >= 0 }
    })
}

/// A bundle's signature: ad-hoc, or the designated requirement of a certificate signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    Adhoc,
    Designated(String),
}

fn codesign(args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/codesign")
        .args(args)
        .output()
        .map_err(|e| UpdateError::new(format!("codesign did not run: {e}")))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let tail: String = text.lines().take(3).collect::<Vec<_>>().join("; ");
        return Err(UpdateError::new(format!("code signature check failed: {tail}")));
    }
    Ok(text)
}

/// `codesign --verify --deep --strict`, then ad-hoc or the designated requirement.
pub fn verify_signed(app: &Path) -> Result<Signature> {
    let path = app.to_string_lossy();
    codesign(&["--verify", "--deep", "--strict", &path])?;
    let details = codesign(&["-dv", "--verbose=4", &path])?;
    if details.contains("Signature=adhoc") {
        return Ok(Signature::Adhoc);
    }
    let requirement = codesign(&["-d", "-r-", &path])?
        .lines()
        .find_map(|l| l.strip_prefix("designated => ").map(str::to_owned))
        .ok_or_else(|| UpdateError::new("bundle signing identity is unavailable"))?;
    Ok(Signature::Designated(requirement))
}

/// The bundle verifies deep and strict against the requirement text.
pub fn satisfies(app: &Path, requirement: &str) -> Result<()> {
    // The leading '=' makes the next argument inline requirement text, not a file path.
    codesign(&["--verify", "--deep", "--strict", "-R", &format!("={requirement}"), &app.to_string_lossy()]).map(|_| ())
}

/// The downloaded app must be signed like the running one, so macOS permissions and Keychain items carry over and
/// nobody else's code replaces Heimdall:
/// - certificate-signed: the same designated requirement, and the download satisfies it (deep, strict);
/// - ad-hoc (development and tests only): the download is ad-hoc too with the product's identifier. Ad-hoc proves no
///   origin; the signed checksums are then the only guard.
pub struct IdentityCheck {
    pub signature: fn(&Path) -> Result<Signature>,
    pub satisfies: fn(&Path, &str) -> Result<()>,
    pub bundle_id: &'static str,
}

impl IdentityCheck {
    pub fn new(product: &Product) -> IdentityCheck {
        IdentityCheck { signature: verify_signed, satisfies, bundle_id: product.bundle_id }
    }

    pub fn check(&self, downloaded: &Path, running: &Path) -> Result<Signature> {
        let current = (self.signature)(running).map_err(|_| {
            UpdateError::new("this app's own signature does not verify, so the download cannot be matched to it")
        })?;
        let replacement = (self.signature)(downloaded)?;
        let mismatch = || {
            UpdateError::new(format!(
                "the download is signed by a different identity than this app ({}, this app: {}); nothing installed",
                describe(&replacement),
                describe(&current)
            ))
        };
        match (&current, &replacement) {
            (Signature::Adhoc, Signature::Adhoc) => {
                (self.satisfies)(downloaded, &format!("identifier \"{}\"", self.bundle_id)).map_err(|_| mismatch())?
            }
            (Signature::Designated(a), Signature::Designated(b)) if a == b => {
                (self.satisfies)(downloaded, a).map_err(|_| mismatch())?
            }
            _ => return Err(mismatch()),
        }
        Ok(replacement)
    }
}

pub fn describe(signature: &Signature) -> String {
    match signature {
        Signature::Adhoc => "ad-hoc".into(),
        Signature::Designated(text) => {
            if let Some(start) = text.find("subject.CN] = \"") {
                let rest = &text[start + "subject.CN] = \"".len()..];
                if let Some(end) = rest.find('"') {
                    return rest[..end].to_owned();
                }
            }
            if let Some(start) = text.find("H\"") {
                return format!("certificate {}", text[start + 2..].chars().take(8).collect::<String>());
            }
            "another certificate".into()
        }
    }
}
