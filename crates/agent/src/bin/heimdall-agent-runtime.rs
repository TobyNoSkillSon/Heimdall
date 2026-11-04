//! Build-time tool for the agent runtime that ships inside Heimdall.app (scripts/bundle.sh). Never run by the app.
//!
//!   heimdall-agent-runtime build <cache>   assemble or reuse <cache>/runtime-<pin>; print its path
//!   heimdall-agent-runtime seal <root>     rewrite the integrity receipt (after signing Mach-O files)
//!   heimdall-agent-runtime verify <root>   the app's integrity check: files, pin, Node and Pi versions
//!
//! The runtime holds only what runs: the pinned `node` executable without local symbols (no npm, headers or docs); from the committed
//! lock (no lifecycle scripts, no bin links) only the packages something loads (`ROOTS` and their dependencies),
//! minus source maps and type declarations; and Heimdall's own runtime files. The Node archive and npm's cache stay in <cache> so a change
//! to Heimdall's files rebuilds offline. <cache>/current points at the newest verified runtime for development.
use agent::runtime::{self, Runtime};
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// What loads from node_modules. Pi's CLI is one bundle (`dist/bundle`) with the provider SDKs compiled in; its
/// only module specifiers that resolve to packages are photon-node, quickjs-wasi, undici, typebox and jiti (the
/// build rescans the bundle and fails on any other). Heimdall's extension imports pi-mcp. Heimdall reads Pi's model
/// catalog files from pi-ai (`provider::pi_catalog`), whose code never loads. `false`: the package's files only.
const ROOTS: &[(&str, bool)] = &[
    ("@earendil-works/pi-coding-agent", false),
    ("@earendil-works/pi-ai", false),
    ("@earendil-works/pi-mcp", true),
    ("@silvia-odwyer/photon-node", true),
    ("quickjs-wasi", true),
    ("undici", true),
    ("typebox", true),
    ("jiti", true),
];

fn main() -> Result<()> {
    if let Some(status) = agent::guardian_mode() {
        std::process::exit(status);
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let (Some(verb), Some(path), None) = (args.first().and_then(|a| a.to_str()), args.get(1), args.get(2)) else {
        bail!("usage: heimdall-agent-runtime build <cache> | seal <root> | verify <root>");
    };
    let path = std::path::absolute(path)?;
    match verb {
        "build" => println!("{}", build(&path)?.display()),
        "seal" => runtime::seal(&path)?,
        "verify" => {
            Runtime::at(path.clone()).verify()?;
            println!("Pinned Pi {} / Node {} verified at {}", runtime::pi_version(), runtime::node_version(), path.display());
        }
        _ => bail!("unknown command {verb}"),
    }
    Ok(())
}

fn build(cache: &Path) -> Result<PathBuf> {
    ensure!(cfg!(all(target_os = "macos", target_arch = "aarch64")), "pinned Node runtime requires Apple Silicon macOS");
    fs::create_dir_all(cache)?;
    let out = cache.join(format!("runtime-{}", &runtime::pin()[..16]));
    let current = cache.join("current");
    if Runtime::at(out.clone()).verify().is_ok() {
        point(&current, &out)?;
        return Ok(out);
    }
    let dist = node_distribution(cache)?;
    let stage = tempfile::Builder::new().prefix("stage-").tempdir_in(cache)?;
    let root = stage.path();
    fs::create_dir_all(root.join("node/bin"))?;
    let node = root.join("node/bin/node");
    fs::copy(dist.join("bin/node"), &node)?;
    // Local symbols only serve debuggers (37 MB of 146 MB); exported ones stay for native addons. Stripping voids
    // the official signature, so the build cache gets an ad-hoc one; scripts/bundle.sh signs with the app identity.
    runtime::checked(Command::new("/usr/bin/strip").arg("-x").arg(&node), Duration::from_secs(60))?;
    runtime::checked(Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(&node), Duration::from_secs(60))?;
    fs::copy(dist.join("LICENSE"), root.join("node/LICENSE"))?;
    for (name, text) in runtime::FILES {
        fs::write(root.join(name), text)?;
    }
    runtime::checked(
        Command::new(dist.join("bin/node"))
            .arg(dist.join("lib/node_modules/npm/bin/npm-cli.js"))
            .args(["ci", "--omit=dev", "--ignore-scripts", "--no-bin-links", "--no-audit", "--no-fund", "--prefer-offline", "--cache"])
            .arg(cache.join("npm-cache"))
            .current_dir(root)
            .env("PATH", format!("{}:/usr/bin:/bin", dist.join("bin").display()))
            .env("npm_config_fetch_retries", "1")
            .env("npm_config_fetch_timeout", "30000"),
        Duration::from_secs(300),
    )?;
    fs::remove_file(root.join("node_modules/.package-lock.json"))?;
    let keep = loaded_packages()?;
    bundle_loads_only(root, &keep)?;
    for package in installed_packages(root)? {
        if !keep.contains(&package) && root.join(&package).exists() {
            fs::remove_dir_all(root.join(&package))?;
        }
    }
    for scope in fs::read_dir(root.join("node_modules"))? {
        let scope = scope?.path();
        if scope.file_name().unwrap().to_string_lossy().starts_with('@') && fs::read_dir(&scope)?.next().is_none() {
            fs::remove_dir(&scope)?;
        }
    }
    exactly(root, &keep)?;
    prune(&root.join("node_modules"))?;
    runtime::seal(root)?;
    Runtime::at(root.to_path_buf()).verify()?;
    if out.exists() {
        fs::remove_dir_all(&out)?;
    }
    fs::rename(stage.keep(), &out)?;
    Runtime::at(out.clone()).verify()?;
    point(&current, &out)?;
    // A cache, not a rollback: older builds' runtimes go.
    for entry in fs::read_dir(cache)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if (name.starts_with("runtime-") || name.starts_with("stage-")) && path != out {
            fs::remove_dir_all(&path)?;
        }
    }
    Ok(out)
}

fn point(link: &Path, target: &Path) -> Result<()> {
    let _ = fs::remove_file(link);
    std::os::unix::fs::symlink(target.file_name().unwrap(), link)?;
    Ok(())
}

/// The verified official archive, extracted once: its npm installs the lock; only `bin/node` ships.
fn node_distribution(cache: &Path) -> Result<PathBuf> {
    let lock: Value = serde_json::from_str(runtime::NODE)?;
    let (url, sha, archive) = (lock["url"].as_str().unwrap(), lock["sha256"].as_str().unwrap(), lock["archive"].as_str().unwrap());
    let dist = cache.join(format!("node-v{}", runtime::node_version()));
    if dist.join("bin/node").is_file() && dist.join("lib/node_modules/npm/bin/npm-cli.js").is_file() && dist.join(".verified").is_file() {
        return Ok(dist);
    }
    let downloads = cache.join("downloads");
    fs::create_dir_all(&downloads)?;
    let tar = downloads.join(archive);
    let digest = |path: &Path| -> Result<String> {
        use sha2::Digest;
        Ok(format!("{:x}", sha2::Sha256::digest(fs::read(path)?)))
    };
    if !tar.is_file() || digest(&tar)? != sha {
        let part = downloads.join(format!("{archive}.part"));
        runtime::checked(
            Command::new("/usr/bin/curl")
                .args(["--fail", "--silent", "--show-error", "--location", "--connect-timeout", "10", "--max-time", "300", "--output"])
                .arg(&part)
                .arg(url),
            Duration::from_secs(305),
        )?;
        ensure!(digest(&part)? == sha, "Node archive integrity mismatch");
        fs::rename(&part, &tar)?;
    }
    if dist.exists() {
        fs::remove_dir_all(&dist)?;
    }
    let stage = tempfile::Builder::new().prefix("node-stage-").tempdir_in(cache)?;
    runtime::checked(
        Command::new("/usr/bin/tar").arg("-xzf").arg(&tar).arg("--strip-components=1").arg("-C").arg(stage.path()),
        Duration::from_secs(60),
    )?;
    fs::write(stage.path().join(".verified"), sha)?;
    fs::rename(stage.keep(), &dist)?;
    Ok(dist)
}

fn lock_packages() -> Result<serde_json::Map<String, Value>> {
    let lock: Value = serde_json::from_str(runtime::LOCK)?;
    Ok(lock["packages"].as_object().context("lock packages")?.clone())
}

/// Node's resolution over the lock's tree: the nearest `node_modules/<name>` from `from` upward.
fn resolve(lock: &serde_json::Map<String, Value>, from: &str, name: &str) -> Option<String> {
    let mut base = from.to_owned();
    loop {
        let key = if base.is_empty() { format!("node_modules/{name}") } else { format!("{base}/node_modules/{name}") };
        if lock.contains_key(&key) {
            return Some(key);
        }
        if base.is_empty() {
            return None;
        }
        base = base.rfind("/node_modules/").map_or(String::new(), |i| base[..i].to_owned());
    }
}

/// Lock keys of `ROOTS` and everything their dependencies resolve to.
fn loaded_packages() -> Result<BTreeSet<String>> {
    let lock = lock_packages()?;
    let mut keep = BTreeSet::new();
    let mut todo = vec![];
    for (name, walk) in ROOTS {
        let key = resolve(&lock, "", name).with_context(|| format!("{name} is not in the lock"))?;
        if *walk { todo.push(key) } else { keep.insert(key); }
    }
    while let Some(key) = todo.pop() {
        if !keep.insert(key.clone()) {
            continue;
        }
        let entry = &lock[&key];
        for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
            for name in entry[field].as_object().into_iter().flat_map(|d| d.keys()) {
                if let Some(dep) = resolve(&lock, &key, name) {
                    todo.push(dep);
                }
            }
        }
    }
    Ok(keep)
}

/// Bare module specifiers in Pi's CLI bundle (`require*("x")`, `import("x")`, `from "x"`).
fn specifiers(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let mut out = BTreeSet::new();
    for (i, &q) in bytes.iter().enumerate() {
        if q != b'"' && q != b'\'' {
            continue;
        }
        let mut j = i;
        while j > 0 && bytes[j - 1].is_ascii_whitespace() { j -= 1; }
        let call = if j > 0 && bytes[j - 1] == b'(' {
            j -= 1;
            while j > 0 && bytes[j - 1].is_ascii_whitespace() { j -= 1; }
            let end = j;
            while j > 0 && ident(bytes[j - 1]) { j -= 1; }
            let name = &text[j..end];
            name == "import" || name.contains("require") || name.contains("Require")
        } else {
            j >= 4 && &bytes[j - 4..j] == b"from" && (j == 4 || !ident(bytes[j - 5]))
        };
        if !call {
            continue;
        }
        let Some(len) = bytes[i + 1..].iter().position(|&c| c == q) else { continue };
        let spec = &text[i + 1..i + 1 + len];
        if spec.is_empty() || spec.starts_with('.') || spec.starts_with('/') || spec.starts_with("node:") || spec.contains(char::is_whitespace) {
            continue;
        }
        let mut parts = spec.split('/');
        let first = parts.next().unwrap();
        out.insert(if first.starts_with('@') { format!("{first}/{}", parts.next().unwrap_or_default()) } else { first.to_owned() });
    }
    out
}

/// Fails the build if Pi's bundle names a locked package that would not ship (a Pi upgrade changed its imports).
fn bundle_loads_only(root: &Path, keep: &BTreeSet<String>) -> Result<()> {
    let lock = lock_packages()?;
    let mut text = String::new();
    fn read(dir: &Path, text: &mut String) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() { read(&path, text)?; } else if path.extension().is_some_and(|e| e == "js") { text.push_str(&fs::read_to_string(&path)?); text.push('\n'); }
        }
        Ok(())
    }
    read(&root.join("node_modules/@earendil-works/pi-coding-agent/dist/bundle"), &mut text)?;
    for name in specifiers(&text) {
        if let Some(key) = resolve(&lock, "node_modules/@earendil-works/pi-coding-agent", &name) {
            ensure!(keep.contains(&key), "Pi's bundle loads {name}, which would not ship; add it to ROOTS");
        }
    }
    Ok(())
}

/// Every package directory under node_modules (scoped and nested), as lock-style keys.
fn installed_packages(root: &Path) -> Result<BTreeSet<String>> {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with('.') || !path.is_dir() {
                continue;
            }
            let packages = if name.starts_with('@') {
                fs::read_dir(&path)?.map(|e| Ok(e?.path())).collect::<Result<Vec<_>>>()?
            } else {
                vec![path]
            };
            for package in packages {
                found.insert(package.strip_prefix(root)?.to_string_lossy().into_owned());
                if package.join("node_modules").is_dir() {
                    walk(root, &package.join("node_modules"), found)?;
                }
            }
        }
        Ok(())
    }
    let mut found = BTreeSet::new();
    walk(root, &root.join("node_modules"), &mut found)?;
    Ok(found)
}

/// The runtime holds exactly the kept packages, each at the lock's version: nothing missing, nothing extra.
fn exactly(root: &Path, keep: &BTreeSet<String>) -> Result<()> {
    let lock = lock_packages()?;
    for key in keep {
        let manifest: Value = serde_json::from_slice(&fs::read(root.join(key).join("package.json")).with_context(|| format!("{key} missing"))?)?;
        ensure!(manifest["version"] == lock[key]["version"], "{key}: installed {} but the lock pins {}", manifest["version"], lock[key]["version"]);
    }
    let found = installed_packages(root)?;
    ensure!(&found == keep, "runtime packages differ: extra {:?}, missing {:?}", found.difference(keep).collect::<Vec<_>>(), keep.difference(&found).collect::<Vec<_>>());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundle_specifiers_cover_aliased_requires_and_skip_relative_and_builtins() {
        let found = super::specifiers(r#"x=require2("jiti").createJiti;import{a}from"typebox/value";await import('quickjs-wasi/q.wasm');require("./c.js");require("node:fs");q.from("nope");y=__require("@scope/pkg/sub");"#);
        assert_eq!(found.into_iter().collect::<Vec<_>>(), ["@scope/pkg", "jiti", "quickjs-wasi", "typebox"]);
    }
}

/// Source maps and type declarations are never loaded by Node; everything else stays.
fn prune(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        ensure!(!meta.file_type().is_symlink(), "unexpected symlink in runtime: {}", path.display());
        if meta.is_dir() {
            prune(&path)?;
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy();
        if [".map", ".d.ts", ".d.mts", ".d.cts", ".tsbuildinfo"].iter().any(|s| name.ends_with(s)) {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
