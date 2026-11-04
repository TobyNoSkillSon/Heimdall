//! Where Heimdall lives on this Mac (docs/home.md).
//!
//! The user picks a home folder in the setup wizard; Heimdall works in it and keeps all of its own files in
//! `<home>/.heimdall`. The only file outside is the pointer that says where home is:
//! `~/Library/Application Support/<bundle id>/home.json`, one per edition, so the public app can never read the
//! personal app's profile.
//!
//! Resolution order: `HEIMDALL_DATA` (and `HEIMDALL_HOME`) from the environment, which the app passes to every
//! child it starts and tests point at temp folders; then the pointer; then, for the personal edition only, the
//! profile from before home folders existed (`~/Library/Application Support/Heimdall`, never moved without the
//! user's yes); otherwise a fresh profile in the user's home folder.
//!
//! Changing home is a move done at the next start, before anything opens a file: `request_move` records it in the
//! pointer, the app restarts, and `apply_pending` performs it. On the same volume it is one rename.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The folder inside home that holds Heimdall's own files.
pub const DIR: &str = ".heimdall";
/// The personal edition's app ID; it never changes, so macOS keeps its permission grants.
pub const PERSONAL_ID: &str = "dev.heimdall.app";
/// The public edition's app ID.
pub const PUBLIC_ID: &str = "com.tobynoskillson.heimdall";
const POINTER: &str = "home.json";

/// What the current process knows about its user and edition. `current()` reads it once; tests build their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    /// The macOS user's home folder (`$HOME`).
    pub user_home: PathBuf,
    pub bundle_id: String,
    /// `HEIMDALL_DATA`: the data folder, set by the app for its children and by tests.
    pub data_override: Option<PathBuf>,
    /// `HEIMDALL_HOME`: the home folder that goes with `HEIMDALL_DATA`.
    pub home_override: Option<PathBuf>,
}

impl Env {
    pub fn current() -> Env {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let mut env = Env {
            user_home: var("HOME").unwrap_or_else(|| PathBuf::from("/tmp")),
            bundle_id: bundle_id(),
            data_override: var("HEIMDALL_DATA"),
            home_override: var("HEIMDALL_HOME"),
        };
        // A cargo test binary that names no folder never resolves to the user's profile: it gets one of its own
        // (31 Oct: an app test run at 22:13 overlapped the archive lock stall; nothing kept its tests off the real
        // archive but each test's own care).
        // `HEIMDALL_TEST_REAL_PROFILE=1`: an ignored replay lab that reads the real profile on purpose.
        if env.data_override.is_none() && var("HEIMDALL_TEST_REAL_PROFILE").is_none() {
            // Only the data folder moves: home (the agent's working folder) is read, never written, by tests.
            if let Some(own) = test_binary_data() {
                env.data_override = Some(own.join(DIR));
            }
        }
        env
    }
    pub fn personal(&self) -> bool {
        self.bundle_id == PERSONAL_ID
    }
    /// `~/Library/Application Support/<bundle id>`: holds only the pointer.
    pub fn pointer_dir(&self) -> PathBuf {
        self.user_home.join("Library/Application Support").join(&self.bundle_id)
    }
    pub fn pointer_path(&self) -> PathBuf {
        self.pointer_dir().join(POINTER)
    }
    /// The personal edition's data folder from before home folders existed.
    pub fn legacy_data(&self) -> PathBuf {
        self.user_home.join("Library/Application Support/Heimdall")
    }
}

/// A per-process folder under the temp folder when this process is a cargo test binary (`target/<profile>/deps/…`).
fn test_binary_data() -> Option<PathBuf> {
    test_folder(&std::env::current_exe().ok()?)
}
fn test_folder(exe: &Path) -> Option<PathBuf> {
    let deps = exe.parent()?;
    let in_target = deps.ancestors().any(|a| a.file_name().is_some_and(|n| n == "target"));
    (deps.file_name()? == "deps" && in_target).then(|| std::env::temp_dir().join(format!("heimdall-test-{}", std::process::id())))
}

/// The app ID of the running bundle: `HEIMDALL_BUNDLE_ID`, else the Info.plist of the app the executable sits in,
/// else the personal edition (cargo runs, the personal workers).
pub fn bundle_id() -> String {
    if let Some(id) = std::env::var("HEIMDALL_BUNDLE_ID").ok().filter(|v| !v.is_empty()) {
        return id;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf))
        .and_then(|app| fs::read_to_string(app.join("Contents/Info.plist")).ok())
        .and_then(|plist| plist_string(&plist, "CFBundleIdentifier"))
        .unwrap_or_else(|| PERSONAL_ID.to_string())
}

fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

/// The pointer file: where home is, and a move waiting for the next start.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub home: PathBuf,
    /// Where the files are while a move waits. `None` means `<home>/.heimdall`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Move>,
}

/// A home change waiting for the next start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub from: PathBuf,
    pub to_home: PathBuf,
    /// Leave a link at `from` pointing at the new folder (the legacy personal profile: other tools still name it).
    #[serde(default)]
    pub leave_link: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `HEIMDALL_DATA` from the environment.
    Env,
    /// The pointer file.
    Pointer,
    /// The personal profile from before home folders: not moved yet.
    Legacy,
    /// Nothing chosen yet: a new profile in the user's home folder.
    Fresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The folder Heimdall treats as home: the agent's working folder.
    pub home: PathBuf,
    /// Heimdall's own files: settings, logs, archive, agent state.
    pub data: PathBuf,
    pub source: Source,
}

impl Location {
    /// True until the user has picked a home folder (the wizard's home step shows).
    pub fn needs_choice(&self) -> bool {
        matches!(self.source, Source::Fresh | Source::Legacy)
    }
}

pub fn read_pointer(env: &Env) -> Option<Pointer> {
    let bytes = fs::read(env.pointer_path()).ok()?;
    serde_json::from_slice::<Pointer>(&bytes).ok().filter(|p| p.home.is_absolute())
}

/// Writes the pointer atomically (temp file, fsync, rename).
pub fn write_pointer(env: &Env, pointer: &Pointer) -> Result<(), String> {
    let dir = env.pointer_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{POINTER}.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(pointer).map_err(|e| e.to_string())?;
    let mut f = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(&bytes).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, env.pointer_path()).map_err(|e| format!("{}: {e}", env.pointer_path().display()))
}

/// Where home and the data folder are for `env`. Never touches the disk beyond reading the pointer.
pub fn resolve_in(env: &Env) -> Location {
    if let Some(data) = &env.data_override {
        let home = env.home_override.clone().or_else(|| read_pointer(env).map(|p| p.home)).unwrap_or_else(|| env.user_home.clone());
        return Location { home, data: data.clone(), source: Source::Env };
    }
    if let Some(p) = read_pointer(env) {
        let mut data = p.data.clone().unwrap_or_else(|| p.home.join(DIR));
        // A move whose files moved but whose pointer was not rewritten: the files are at the new place.
        if let Some(m) = p.pending.as_ref().filter(|_| !data.exists()) {
            if m.to_home.join(DIR).is_dir() {
                data = m.to_home.join(DIR);
            }
        }
        return Location { home: p.home, data, source: Source::Pointer };
    }
    if env.personal() && env.legacy_data().is_dir() {
        // The personal edition's first home was its owner's notes folder when there was one.
        let notes = env.user_home.join("Vault");
        let home = if notes.is_dir() { notes } else { env.user_home.clone() };
        return Location { home, data: env.legacy_data(), source: Source::Legacy };
    }
    Location { home: env.user_home.clone(), data: env.user_home.join(DIR), source: Source::Fresh }
}

static CURRENT: OnceLock<Location> = OnceLock::new();

/// This process's location, read once. The app applies a pending move before its first call.
pub fn current() -> &'static Location {
    CURRENT.get_or_init(|| resolve_in(&Env::current()))
}
/// Fixes this process's location before anything reads it: the app sets it once at start, after `apply_pending` and
/// its `--data-dir`, so `current().source` still says whether a home was picked. False if it was already read.
pub fn init(loc: Location) -> bool {
    CURRENT.set(loc).is_ok()
}
pub fn data_dir() -> PathBuf {
    current().data.clone()
}
pub fn home_dir() -> PathBuf {
    current().home.clone()
}

/// What a home change will do, shown before the user confirms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub to_home: PathBuf,
    /// One rename on the same volume; otherwise a copy, a check and then removal of the old folder.
    pub same_volume: bool,
    /// Measured size of `from` in bytes (0 when it does not exist yet).
    pub bytes: u64,
    pub leave_link: bool,
}

/// Checks a new home folder against the current location and says what moving there will do. Writes nothing.
pub fn plan_move(env: &Env, current: &Location, new_home: &Path) -> Result<MovePlan, String> {
    if !new_home.is_absolute() {
        return Err("Choose a full folder path.".into());
    }
    let new_home = fs::canonicalize(new_home).map_err(|_| format!("{} does not exist.", new_home.display()))?;
    if !new_home.is_dir() {
        return Err(format!("{} is not a folder.", new_home.display()));
    }
    let to = new_home.join(DIR);
    // A data folder that is a link moves the folder it points at.
    let from_real = fs::canonicalize(&current.data).unwrap_or_else(|_| current.data.clone());
    let from = from_real.clone();
    if to == from_real || to == from {
        return Err("Heimdall already lives there.".into());
    }
    if new_home.starts_with(&from_real) {
        return Err("Home can't be inside Heimdall's own folder.".into());
    }
    if fs::read_dir(&to).map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!("{} already has a .heimdall folder; move or remove it first.", new_home.display()));
    }
    let probe = new_home.join(format!(".heimdall-probe-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|_| format!("Heimdall can't write in {}.", new_home.display()))?;
    let _ = fs::remove_file(&probe);
    let same_volume = !from.exists() || device(&from) == device(&new_home);
    Ok(MovePlan {
        bytes: size_of(&from),
        same_volume,
        leave_link: current.source == Source::Legacy && env.personal(),
        from,
        to,
        to_home: new_home,
    })
}

/// Records the move in the pointer; it happens at the next start (`apply_pending`). The caller restarts the app.
pub fn request_move(env: &Env, current: &Location, plan: &MovePlan) -> Result<(), String> {
    let pointer = Pointer {
        home: current.home.clone(),
        data: Some(plan.from.clone()),
        pending: Some(Move { from: plan.from.clone(), to_home: plan.to_home.clone(), leave_link: plan.leave_link }),
    };
    write_pointer(env, &pointer)
}

/// Records a home with nothing to move (a fresh profile whose folder doesn't exist yet).
pub fn set_home(env: &Env, home: &Path) -> Result<(), String> {
    write_pointer(env, &Pointer { home: home.to_path_buf(), data: None, pending: None })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    Renamed { from: PathBuf, to: PathBuf },
    Copied { from: PathBuf, to: PathBuf, bytes: u64 },
    /// A move interrupted after the files moved: only the pointer was left to write.
    Finished { to: PathBuf },
    /// Nothing existed at `from`; the new home starts empty.
    Empty { to: PathBuf },
}

/// Performs a pending move. Call once at start, before anything opens a file in the data folder. On failure the
/// pending move is dropped, the files stay where they were and the pointer keeps the old location.
pub fn apply_pending(env: &Env) -> Result<Option<Applied>, String> {
    let Some(pointer) = read_pointer(env) else { return Ok(None) };
    let Some(m) = pointer.pending.clone() else { return Ok(None) };
    let to = m.to_home.join(DIR);
    let done = |env: &Env| write_pointer(env, &Pointer { home: m.to_home.clone(), data: None, pending: None });
    let abandon = |env: &Env, why: String| -> Result<Option<Applied>, String> {
        let _ = write_pointer(env, &Pointer { pending: None, ..pointer.clone() });
        Err(why)
    };
    let from_present = fs::symlink_metadata(&m.from).is_ok_and(|md| !md.file_type().is_symlink());
    if !from_present {
        done(env)?;
        if to.exists() {
            if m.leave_link && fs::symlink_metadata(&m.from).is_err() {
                let _ = std::os::unix::fs::symlink(&to, &m.from);
            }
            repoint_legacy_link(env, &m.from, &to);
            return Ok(Some(Applied::Finished { to }));
        }
        return Ok(Some(Applied::Empty { to }));
    }
    if fs::read_dir(&to).map(|mut d| d.next().is_some()).unwrap_or(false) {
        // A copy across volumes interrupted after the copy was checked and renamed into place: finish it.
        if tree_stats(&to).ok().is_some() && tree_stats(&to).ok() == tree_stats(&m.from).ok() && device(&m.from) != device(&m.to_home) {
            done(env)?;
            finish_copied(&m, &to)?;
            return Ok(Some(Applied::Finished { to }));
        }
        return abandon(env, format!("{} is not empty; nothing was moved.", to.display()));
    }
    let _ = fs::remove_dir(&to);
    let applied = if device(&m.from) == device(&m.to_home) {
        if let Err(e) = fs::rename(&m.from, &to) {
            return abandon(env, format!("Moving {} failed: {e}", m.from.display()));
        }
        Applied::Renamed { from: m.from.clone(), to: to.clone() }
    } else {
        let partial = m.to_home.join(format!("{DIR}.partial"));
        let _ = fs::remove_dir_all(&partial);
        let copied = copy_tree(&m.from, &partial).and_then(|bytes| {
            let (want, got) = (tree_stats(&m.from)?, tree_stats(&partial)?);
            if want != got {
                return Err(format!("the copy differs from the original ({want:?} vs {got:?})"));
            }
            Ok(bytes)
        });
        let bytes = match copied.and_then(|b| fs::rename(&partial, &to).map(|_| b).map_err(|e| e.to_string())) {
            Ok(b) => b,
            Err(e) => {
                let _ = fs::remove_dir_all(&partial);
                return abandon(env, format!("Copying to {} failed: {e}", to.display()));
            }
        };
        // The pointer moves first, so a crash here leaves a usable copy rather than none.
        done(env)?;
        finish_copied(&m, &to)?;
        repoint_legacy_link(env, &m.from, &to);
        return Ok(Some(Applied::Copied { from: m.from, to, bytes }));
    };
    done(env)?;
    if m.leave_link {
        let _ = std::os::unix::fs::symlink(&to, &m.from);
    }
    repoint_legacy_link(env, &m.from, &to);
    Ok(Some(applied))
}

/// After a checked copy across volumes: the legacy profile is kept beside a link until its owner removes it; any
/// other old folder is removed.
fn finish_copied(m: &Move, to: &Path) -> Result<(), String> {
    if m.leave_link {
        let kept = m.from.with_file_name(format!("{}-before-move", m.from.file_name().unwrap_or_default().to_string_lossy()));
        let _ = fs::rename(&m.from, kept);
        let _ = std::os::unix::fs::symlink(to, &m.from);
        Ok(())
    } else {
        fs::remove_dir_all(&m.from).map_err(|e| format!("{} was copied but not removed: {e}", m.from.display()))
    }
}

/// The legacy link from an earlier move keeps pointing at the files when they move again.
fn repoint_legacy_link(env: &Env, from: &Path, to: &Path) {
    // Only the personal edition has a legacy profile; the public one never looks at its path (VM finding P-08).
    if !env.personal() {
        return;
    }
    let legacy = env.legacy_data();
    if legacy.as_path() == from || !fs::symlink_metadata(&legacy).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    if fs::read_link(&legacy).is_ok_and(|t| t == from) {
        let tmp = legacy.with_file_name(".Heimdall-link");
        let _ = fs::remove_file(&tmp);
        if std::os::unix::fs::symlink(to, &tmp).is_ok() {
            let _ = fs::rename(&tmp, &legacy);
        }
    }
}

fn device(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).ok().map(|m| m.dev())
}

/// Bytes of regular files under `p`, not following links.
pub fn size_of(p: &Path) -> u64 {
    tree_stats(p).map(|(_, bytes)| bytes).unwrap_or(0)
}

/// (entries, bytes of regular files) under `p`, not following links.
fn tree_stats(p: &Path) -> Result<(u64, u64), String> {
    let md = match fs::symlink_metadata(p) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    if !md.is_dir() {
        return Ok((1, if md.is_file() { md.len() } else { 0 }));
    }
    let mut total = (1, 0);
    for entry in fs::read_dir(p).map_err(|e| format!("{}: {e}", p.display()))? {
        let (n, b) = tree_stats(&entry.map_err(|e| e.to_string())?.path())?;
        total = (total.0 + n, total.1 + b);
    }
    Ok(total)
}

/// Copies a tree keeping links as links and permissions; returns the bytes copied.
fn copy_tree(from: &Path, to: &Path) -> Result<u64, String> {
    let md = fs::symlink_metadata(from).map_err(|e| format!("{}: {e}", from.display()))?;
    if md.file_type().is_symlink() {
        let target = fs::read_link(from).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(target, to).map_err(|e| format!("{}: {e}", to.display()))?;
        return Ok(0);
    }
    if md.is_dir() {
        fs::create_dir(to).map_err(|e| format!("{}: {e}", to.display()))?;
        let mut bytes = 0;
        for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            bytes += copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(to, md.permissions()).map_err(|e| e.to_string())?;
        return Ok(bytes);
    }
    let bytes = fs::copy(from, to).map_err(|e| format!("{}: {e}", from.display()))?;
    fs::File::open(to).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
