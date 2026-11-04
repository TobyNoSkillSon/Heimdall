//! The only things an agent on this Mac may not do without the user ("the agents get the most YOLO mode
//! possible, so they literally don't ask you anything"; docs/harness-autonomy.md). Everything else is approved.
//!
//! The deny list, short on purpose:
//! 1. **Deleting outside the Vault and scratch.** `rm`, `rmdir`, `unlink`, `srm`, `find … -delete` / `-exec rm`, and
//!    `rsync --delete` whose target is not inside the Vault (two folders deep: `<Vault>/Projects/X`, never `Projects`
//!    itself), a temporary folder (`/tmp`, `/private/tmp`, `/var/folders`, `$TMPDIR`) or a build cache
//!    (`~/Library/Developer/Xcode/DerivedData`, `~/Library/Caches`, `~/.cache`, `~/.t3/worktrees`, `~/.t3/scratch`),
//!    one folder below it. A glob deletes inside its folder, so that folder itself must be deletable. A target the
//!    shell could expand to the root (`"$DIR"/*`, `$DIR/` with no `${DIR:?}` guard) is refused with how to guard it.
//! 2. **Disk formatting.** `diskutil` erase, partition, zero and APFS delete verbs, `newfs*`, `fdisk`, `gpt`, `asr`,
//!    and `dd` or a redirect onto `/dev/disk*` / `/dev/rdisk*`.
//! 3. **The Keychain.** Every `security` verb but the read-only certificate ones (`find-identity`, `find-certificate`,
//!    `list-keychains`, `show-keychain-info`, `verify-cert`, `cms`, `error`, `help`), and any tool touching
//!    `~/Library/Keychains` or `/Library/Keychains`.
//! 4. **System settings.** `sudo`, `systemsetup`, `csrutil`, `nvram` writes, `fdesetup`, `spctl` changes, `tccutil`,
//!    `pmset` writes, `scutil --set`, `networksetup` changes, `defaults write/delete/import` on Apple or global domains,
//!    `launchctl` changes (bootstrap, bootout, load, unload, enable, disable, kickstart, kill, setenv, remove, submit,
//!    config), `softwareupdate`, `profiles`, `sysadminctl`, `dscl` changes, `kextload`, `shutdown`, `reboot`, `halt`,
//!    login-item changes through `osascript`/`sfltool`, and writes into `LaunchAgents`, `LaunchDaemons`, `/etc`,
//!    `/System`, `/Library/Preferences` or `~/Library/Preferences/com.apple.*`. Reading the login items through System
//!    Events needs no yes (bug note 2026-11-03T18-04-11); `sfltool dumpbtm` stays refused, as it asks for an admin
//!    password on the user's screen.
//! A `mv` of something outside the allowed areas into a temporary folder or a build cache counts as a delete (the
//! mv-then-rm workaround, same note).
//!
//! The user's click on an ask card can grant exact deletes and login-item removals for a short time (`grants.rs`, on in the
//! personal edition: the card draws each one and only his own pointer's click counts): a command whose every refusal is one of those
//! scopes, exactly, is let through (`refuse_with`). Keystrokes, point clicks and presses on Heimdall are refused.
//!
//! This reads the command the way a careful person would, not the way a shell does: it is a guard against an agent's
//! mistake, not a security boundary (a script in Python can still delete anything). Claude Code's own deny rules in
//! `harness.rs` repeat the plainest forms so they hold even without this hook.
use serde_json::Value;

/// The four groups above as Setup's coding-agents step shows them, one line each, in the user's words (the Vault is the
/// home folder they chose in Setup). Only Claude agents are guarded (`harness::GUARDED`).
pub const SUMMARY: [&str; 4] = ["No deletes outside your home folder", "No disk erasing", "No Keychain access", "No system settings"];
use std::path::{Component, Path, PathBuf};
use crate::grants::Scope;

/// Why this call is refused, or None to approve it. `vault` is the Vault root, `user` the user's home folder, `cwd` the
/// session's working folder when known.
pub fn refuse(tool: &str, input: &Value, vault: &Path, user: &Path, cwd: Option<&Path>) -> Option<String> {
    refuse_with(tool, input, vault, user, cwd, &|_| false)
}
/// `refuse`, with `granted` saying whether the user's click covers one scope: an exact delete target or a login item's
/// removal it would refuse otherwise. Everything else on the list is refused as before.
pub fn refuse_with(tool: &str, input: &Value, vault: &Path, user: &Path, cwd: Option<&Path>, granted: &dyn Fn(&Scope) -> bool) -> Option<String> {
    let ctx = Ctx { vault, user, granted };
    match tool {
        "Bash" => input["command"].as_str().and_then(|c| ctx.command(c, cwd.map(Path::to_path_buf), 0)),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let path = input["file_path"].as_str().or(input["notebook_path"].as_str())?;
            let path = ctx.absolute(path, cwd)?;
            ctx.keychain(&path).or_else(|| ctx.system_path(&path))
        }
        "Read" | "Grep" | "Glob" => {
            let path = input["file_path"].as_str().or(input["path"].as_str())?;
            ctx.keychain(&ctx.absolute(path, cwd)?)
        }
        _ => None,
    }
}

const DELETE: &str = "Heimdall's deny list (docs/harness-autonomy.md): this deletes outside the Vault, the temporary \
folders and the build caches. If it really must go, ask User in your final reply, naming each exact path.";
const GUARD: &str = "Heimdall's deny list: if the variable were empty this would delete the root or a folder outside \
the Vault and scratch. Guard it as \"${NAME:?}\" or use the literal path, then run it again.";
const WEIGHTS: &str = "Heimdall's deny list: this deletes model weights, which needs User's yes. Ask in your final reply.";
const DISK: &str = "Heimdall's deny list: formatting, partitioning or writing raw disks needs User's yes. Ask in your \
final reply.";
const KEYCHAIN: &str = "Heimdall's deny list: the Keychain is User's. Reading or changing it needs his yes (each read \
can put a password prompt on his screen). Ask in your final reply.";
const SYSTEM: &str = "Heimdall's deny list: this changes macOS system settings, launch agents or login items, which \
needs User's yes. Ask in your final reply. Reading the login items needs none.";
const PRESS: &str = "Heimdall's deny list: keystrokes, clicks at a point and presses on Heimdall act on User's screen and \
his cards, and only he answers those. Drive an app through its own test hooks or accessibility on a lab build.";
const BTM: &str = "Heimdall's deny list: sfltool dumpbtm asks for an admin password on User's screen. List login items \
with osascript -e 'tell application \"System Events\" to get the name of every login item', launchctl list or \
ls ~/Library/LaunchAgents instead; those need no yes.";

struct Ctx<'a> { vault: &'a Path, user: &'a Path, granted: &'a dyn Fn(&Scope) -> bool }

/// One simple command: its words (quotes removed) and whether each word had an expansion (`$`, backticks) outside
/// single quotes.
#[derive(Debug, Default, Clone)]
struct Simple { words: Vec<String>, expands: Vec<bool>, redirects: Vec<String> }

impl Ctx<'_> {
    fn command(&self, command: &str, mut cwd: Option<PathBuf>, depth: usize) -> Option<String> {
        if depth > 4 { return None; }
        for simple in split(command) {
            let simple = strip_wrappers(simple);
            let Some(program) = simple.words.first() else { continue };
            let name = program.rsplit('/').next().unwrap_or(program).to_owned();
            let args = &simple.words[1..];
            for target in &simple.redirects {
                if target.starts_with("/dev/disk") || target.starts_with("/dev/rdisk") { return Some(DISK.into()); }
                if let Some(p) = self.absolute(target, cwd.as_deref()) {
                    if let Some(r) = self.keychain(&p).or_else(|| self.system_path(&p)) { return Some(r); }
                }
            }
            if name == "cd" {
                cwd = match args.first() {
                    Some(a) if !simple.expands.get(1).copied().unwrap_or(false) => self.absolute(a, cwd.as_deref()),
                    None => Some(self.user.to_path_buf()),
                    _ => None,
                };
                continue;
            }
            if matches!(name.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh") {
                if let Some(i) = args.iter().position(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))) {
                    if let Some(r) = args.get(i + 1).and_then(|s| self.command(s, cwd.clone(), depth + 1)) { return Some(r); }
                }
                continue;
            }
            if name == "eval" {
                if let Some(r) = self.command(&args.join(" "), cwd.clone(), depth + 1) { return Some(r); }
                continue;
            }
            if name == "osascript" && { let all = args.join(" ").to_lowercase(); all.contains("login item") || all.contains("loginitem") } {
                let script = osascript_script(args);
                match login_item_removal(&script) {
                    Some(item) if (self.granted)(&Scope::LoginItem(item.clone())) => continue,
                    Some(_) => return Some(SYSTEM.into()),
                    None if login_item_writes(&script) => return Some(SYSTEM.into()),
                    None => {}
                }
            }
            if name == "sfltool" && args.first().is_some_and(|a| a == "dumpbtm") { return Some(BTM.into()); }
            if name == "cliclick" || (name == "osascript" && presses(&osascript_script(args))) { return Some(PRESS.into()); }
            if name == "mv" { if let Some(r) = self.moves_to_scratch(args, cwd.as_deref(), command.contains("mktemp")) { return Some(r); } }
            if let Some(r) = self.disk(&name, args).or_else(|| self.security(&name, args)).or_else(|| self.system(&name, args)) {
                return Some(r);
            }
            if let Some(r) = self.deletes(&name, &simple, cwd.as_deref()) { return Some(r); }
            // Any other program handed a Keychain file.
            for a in args {
                if let Some(p) = self.absolute(a, cwd.as_deref()) {
                    if let Some(r) = self.keychain(&p) { return Some(r); }
                }
            }
            // `sed` writes only with -i, `plutil` only with an editing verb; otherwise they read.
            let edits = match name.as_str() {
                "sed" => args.iter().any(|a| a.starts_with("-i") || a == "--in-place" || a.starts_with("-Ei") || a.starts_with("-ni")),
                "plutil" => args.iter().any(|a| matches!(a.as_str(), "-convert" | "-insert" | "-replace" | "-remove" | "-create")),
                _ => true,
            };
            if edits && matches!(name.as_str(), "cp" | "mv" | "ln" | "tee" | "install" | "ditto" | "rsync" | "touch" | "mkdir" | "chmod" | "chown" | "plutil" | "sed") {
                for a in args.iter().filter(|a| !a.starts_with('-')) {
                    if let Some(p) = self.absolute(a, cwd.as_deref()) {
                        // A copy *from* a protected place is a read; only the last argument (or tee's/sed's files) is written.
                        let written = matches!(name.as_str(), "tee" | "touch" | "mkdir" | "chmod" | "chown" | "plutil" | "sed") || Some(a) == args.last();
                        if written { if let Some(r) = self.system_path(&p) { return Some(r); } }
                    }
                }
            }
        }
        None
    }

    fn disk(&self, name: &str, args: &[String]) -> Option<String> {
        let verb = args.first().map(|a| a.to_lowercase()).unwrap_or_default();
        let refused = match name {
            "diskutil" => {
                let apfs = verb == "apfs" && args.get(1).is_some_and(|v| { let v = v.to_lowercase(); v.starts_with("delete") || v.starts_with("erase") });
                apfs || ["erase", "partition", "zero", "random", "secureerase", "reformat", "splitpartition", "mergepartitions", "resizevolume"].iter().any(|v| verb.starts_with(v))
            }
            "fdisk" | "gpt" | "asr" | "pdisk" => true,
            n if n.starts_with("newfs") => true,
            "dd" => args.iter().any(|a| a.starts_with("of=/dev/disk") || a.starts_with("of=/dev/rdisk")),
            _ => false,
        };
        refused.then(|| DISK.into())
    }

    fn security(&self, name: &str, args: &[String]) -> Option<String> {
        if name != "security" { return None; }
        const READ_ONLY: [&str; 8] = ["find-identity", "find-certificate", "list-keychains", "show-keychain-info", "verify-cert", "cms", "error", "help"];
        let verb = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("help");
        (!READ_ONLY.contains(&verb)).then(|| KEYCHAIN.into())
    }

    fn system(&self, name: &str, args: &[String]) -> Option<String> {
        let first = args.first().map(String::as_str).unwrap_or("");
        let refused = match name {
            "sudo" | "doas" | "systemsetup" | "csrutil" | "fdesetup" | "tccutil" | "softwareupdate" | "profiles" | "sysadminctl"
            | "kextload" | "kextunload" | "kmutil" | "systemextensionsctl" | "shutdown" | "reboot" | "halt" | "sfltool" | "bless" => {
                !(name == "softwareupdate" && matches!(first, "-l" | "--list" | "--history")) && !(name == "fdesetup" && first == "status") && !(name == "csrutil" && first == "status")
            }
            "nvram" => args.iter().any(|a| a.contains('=') || a == "-d" || a == "-c" || a == "-f"),
            "spctl" => !args.iter().any(|a| matches!(a.as_str(), "--assess" | "-a" | "--status" | "--version")) || args.iter().any(|a| a.contains("disable") || a.contains("enable") || a == "--add" || a == "--remove"),
            "pmset" => !(first == "-g" || first.is_empty()),
            "scutil" => args.iter().any(|a| a == "--set"),
            "networksetup" => args.iter().any(|a| ["-set", "-remove", "-create", "-delete", "-add", "-order", "-switch"].iter().any(|v| a.starts_with(v))),
            "defaults" => {
                let writes = matches!(first, "write" | "delete" | "import" | "rename");
                let host = args.iter().any(|a| a == "-currentHost" || a == "-host");
                let domain = args.iter().skip(1).find(|a| !a.starts_with('-') || matches!(a.as_str(), "-g" | "-globalDomain")).map(String::as_str).unwrap_or("");
                writes && (host || matches!(domain, "-g" | "-globalDomain" | "NSGlobalDomain") || (domain.starts_with("com.apple.") && !domain.starts_with("com.apple.dt.")) || domain.starts_with("/Library/") || domain.contains("Library/Preferences/com.apple."))
            }
            "launchctl" => matches!(first, "bootstrap" | "bootout" | "load" | "unload" | "enable" | "disable" | "kickstart" | "kill" | "setenv" | "unsetenv" | "remove" | "submit" | "config" | "start" | "stop"),
            "dscl" => args.iter().any(|a| matches!(a.as_str(), "-create" | "-delete" | "-append" | "-merge" | "-passwd" | "-change")),
            _ => false,
        };
        refused.then(|| SYSTEM.into())
    }

    /// A delete's targets, each judged by `deletable`.
    fn deletes(&self, name: &str, simple: &Simple, cwd: Option<&Path>) -> Option<String> {
        let words = &simple.words;
        let targets: Vec<usize> = match name {
            "rm" | "rmdir" | "unlink" | "srm" => {
                let mut options = true;
                (1..words.len()).filter(|&i| {
                    let w = &words[i];
                    if options && w == "--" { options = false; return false; }
                    !(options && w.starts_with('-') && w.len() > 1)
                }).collect()
            }
            "find" if words.iter().any(|w| w == "-delete") || words.windows(2).any(|w| w[0] == "-exec" || w[0] == "-execdir") && words.iter().any(|w| matches!(w.as_str(), "rm" | "/bin/rm" | "rmdir")) => {
                // The start points: the words after `find`'s own options and before the first expression.
                (1..words.len()).skip_while(|&i| matches!(words[i].as_str(), "-H" | "-L" | "-P" | "-E" | "-X" | "-d" | "-s" | "-x"))
                    .take_while(|&i| !words[i].starts_with('-') && !matches!(words[i].as_str(), "(" | "!")).collect()
            }
            "rsync" if words.iter().any(|w| w.starts_with("--delete") || w == "--remove-source-files") => {
                words.iter().rposition(|w| !w.starts_with('-')).filter(|&i| i > 0).into_iter().collect()
            }
            _ => return None,
        };
        for i in targets {
            let raw = &words[i];
            let home = raw.starts_with("$HOME") || raw.starts_with("${HOME}");
            if (raw.starts_with('$') || raw.starts_with('`')) && !home {
                // Unknowable until the shell runs it; refuse only the form that becomes the root when it is empty.
                if unguarded_root(raw) { return Some(GUARD.into()); }
                continue;
            }
            // A variable or a bare glob deletes inside the folder before it, so that folder is what is judged.
            let (prefix, by_variable) = cut(raw);
            let Some(path) = self.absolute(&prefix, cwd) else { continue };
            let refusal = if path.starts_with(self.user.join(".cache/huggingface")) { Some(WEIGHTS) }
                else if !self.deletable(&path) { Some(if by_variable { GUARD } else { DELETE }) }
                else { None };
            let Some(refusal) = refusal else { continue };
            // The user's click covers exactly the path he was shown, and only for a plain delete (review 4 Nov: `find -L` and
            // `rsync` reach past it): one absolute literal target, never a glob, a variable, a path from the working
            // folder, a trailing slash (through a symlink it deletes the target) or a symlinked folder on the way.
            if grantable_target(name, words, raw, &path) && prefix == *raw && (self.granted)(&Scope::Delete(path)) { continue; }
            return Some(refusal.into());
        }
        None
    }

    /// `mv` of something outside the allowed areas into a temporary folder: a delete in two steps.
    /// `mktemp`: the command makes a temporary folder, so a substitution as the destination is one.
    fn moves_to_scratch(&self, args: &[String], cwd: Option<&Path>, mktemp: bool) -> Option<String> {
        let operands: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        let (dest, sources) = operands.split_last()?;
        let temp_word = dest.starts_with("$TMPDIR") || dest.starts_with("${TMPDIR") || (mktemp && dest.starts_with("$("));
        let dest = if temp_word { PathBuf::from("/tmp/x") } else { self.absolute(dest, cwd)? };
        // Only the temporary folders: a move into a build cache (`~/.cache/huggingface`) is ordinary work.
        let mut temps: Vec<PathBuf> = ["/tmp", "/private/tmp", "/var/folders", "/private/var/folders"].iter().map(PathBuf::from).collect();
        if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) { temps.push(lexical(Path::new(&t))); }
        let scratch = temps.iter().any(|t| lexical(&dest).starts_with(t));
        if !scratch { return None; }
        for s in sources {
            let (prefix, _) = cut(s);
            let Some(path) = self.absolute(&prefix, cwd) else { continue };
            if !self.deletable(&path) { return Some(DELETE.into()); }
        }
        None
    }

    /// Inside an allowed area, deep enough, as written and with its folders' symlinks resolved.
    fn deletable(&self, path: &Path) -> bool {
        let path = lexical(path);
        let roots = self.roots();
        let inside = |p: &Path| roots.iter().any(|(root, depth)| p.strip_prefix(root).is_ok_and(|rest| rest.components().count() >= *depth));
        inside(&path) && inside(&resolved(&path))
    }

    /// The allowed areas and how many folders below each a delete must start.
    fn roots(&self) -> Vec<(PathBuf, usize)> {
        let mut roots = vec![(self.vault.to_path_buf(), 2)];
        for t in ["/tmp", "/private/tmp", "/var/folders", "/private/var/folders"] { roots.push((PathBuf::from(t), 1)); }
        if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) { roots.push((lexical(Path::new(&t)), 1)); }
        for c in ["Library/Developer/Xcode/DerivedData", "Library/Caches", ".cache", ".t3/worktrees", ".t3/scratch"] { roots.push((self.user.join(c), 1)); }
        let resolved: Vec<(PathBuf, usize)> = roots.iter().filter_map(|(r, d)| std::fs::canonicalize(r).ok().map(|r| (r, *d))).collect();
        roots.extend(resolved);
        roots
    }

    fn keychain(&self, path: &Path) -> Option<String> {
        let p = lexical(path);
        (p.starts_with(self.user.join("Library/Keychains")) || p.starts_with("/Library/Keychains")).then(|| KEYCHAIN.into())
    }

    fn system_path(&self, path: &Path) -> Option<String> {
        let p = lexical(path);
        let user_prefs = p.strip_prefix(self.user.join("Library/Preferences")).ok().and_then(|r| r.to_str()).is_some_and(|r| r.starts_with("com.apple.") || r.starts_with(".GlobalPreferences"));
        let protected = p.starts_with(self.user.join("Library/LaunchAgents"))
            || ["/Library/LaunchAgents", "/Library/LaunchDaemons", "/etc", "/private/etc", "/System", "/Library/Preferences"].iter().any(|r| p.starts_with(r))
            || user_prefs;
        protected.then(|| SYSTEM.into())
    }

    /// A word as an absolute path (`~` and `$HOME` expanded, relative ones from `cwd`); None when it cannot be known.
    fn absolute(&self, word: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        let home_rel = word.strip_prefix("~/").or_else(|| word.strip_prefix("$HOME/")).or_else(|| word.strip_prefix("${HOME}/"));
        let path = if let Some(rest) = home_rel { self.user.join(rest) }
            else if matches!(word, "~" | "$HOME" | "${HOME}") { self.user.to_path_buf() }
            else if word.starts_with('/') { PathBuf::from(word) }
            else if word.is_empty() || word.starts_with('~') || word.contains('$') { return None }
            else { cwd?.join(word) };
        Some(normalize(&path))
    }
}

/// Whether a delete target is one literal path: no variable but a leading `$HOME`, no substitution, no glob.
fn exact(raw: &str) -> bool {
    let rest = raw.strip_prefix("${HOME}").or_else(|| raw.strip_prefix("$HOME")).unwrap_or(raw);
    !rest.contains(['$', '`', '*', '?', '[', ']', '{', '}'])
}

/// The AppleScript an `osascript` call runs: its `-e` lines, or its arguments when there are none.
fn osascript_script(args: &[String]) -> String {
    let lines: Vec<&str> = args.windows(2).filter(|w| w[0] == "-e").map(|w| w[1].as_str()).collect();
    if lines.is_empty() { args.join(" ") } else { lines.join("\n") }
}
fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    s.get(..p.len()).filter(|h| h.eq_ignore_ascii_case(p)).map(|_| &s[p.len()..])
}
fn strip_suffix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    let at = s.len().checked_sub(p.len())?;
    s.get(at..).filter(|t| t.eq_ignore_ascii_case(p)).map(|_| &s[..at])
}
/// The login item a script removes, when the whole script is one System Events removal by name:
/// `tell application "System Events" to delete login item "X"` (or `delete every login item whose name is "X"`, with or
/// without parentheses, also as a `tell … end tell` block).
fn login_item_removal(script: &str) -> Option<String> {
    let s = script.split_whitespace().collect::<Vec<_>>().join(" ");
    let body = strip_prefix_ci(&s, "tell application \"System Events\" to ")
        .or_else(|| strip_prefix_ci(&s, "tell application \"System Events\" ").and_then(|b| strip_suffix_ci(b, " end tell")))?
        .trim();
    for (form, close) in [("delete login item \"", ""), ("delete every login item whose name is \"", ""), ("delete (every login item whose name is \"", ")")] {
        let Some(rest) = strip_prefix_ci(body, form) else { continue };
        let end = rest.find('"')?;
        let (name, tail) = (&rest[..end], rest[end + 1..].trim());
        return (tail == close && !name.trim().is_empty() && !name.contains('\\')).then(|| name.to_owned());
    }
    None
}
/// Whether a script that names login items changes them: a `delete`, `make`, `move`, `duplicate`, `remove`, `create` or
/// `add` outside its strings, or a `set … of … to` (a property). `get` and `set x to (name of every login item)` read.
/// Review 4 Nov: `tell login item "X" to set hidden to true` and `set login item "X"'s hidden to true` write too, so a
/// `set T to` writes when T names a property or a login item; JXA's `loginItems…delete()` and raw «event» codes count.
fn login_item_writes(script: &str) -> bool {
    let words = script_words(script);
    if words.iter().any(|w| matches!(w.as_str(), "delete" | "make" | "move" | "duplicate" | "remove" | "create" | "add" | "push" | "splice")) { return true; }
    let property = |w: &String| matches!(w.as_str(), "of" | "s" | "login" | "item" | "hidden" | "path" | "name" | "kind" | "properties");
    if words.iter().enumerate().filter(|(_, w)| *w == "set").any(|(i, _)| words[i + 1..].iter().take_while(|w| *w != "to").any(property)) { return true; }
    let lower = script.to_lowercase();
    script.contains('\u{ab}') || (lower.contains("loginitem") && lower.contains('='))
}

/// Whether a refused delete target can be one of the user's grants (review 4 Nov): only a plain `rm`, `unlink` or `rmdir`
/// (never `rmdir -p`, `find` or `rsync`, which reach past it), written as one absolute literal path from `/` (never
/// `~`, `$HOME`, the working folder, a glob or a variable), with no `.`, `..` or empty component, no trailing slash
/// (through a symlink it deletes the target) and no symlinked folder on the way.
fn grantable_target(name: &str, words: &[String], raw: &str, path: &Path) -> bool {
    let parents = name == "rmdir" && words.iter().any(|w| w == "--parents" || (w.starts_with('-') && !w.starts_with("--") && w.contains('p')));
    matches!(name, "rm" | "rmdir" | "unlink") && !parents
        && raw.starts_with('/') && exact(raw)
        && !raw[1..].split('/').any(|c| c.is_empty() || c == "." || c == "..")
        && resolved(path) == path
}

/// The programs a command line runs, wrappers stripped (`nice -n 19 rm` is `rm`). Granted commands may hold only
/// a short read-only list beside the delete (`approvals::granted`).
pub fn programs(command: &str) -> Vec<String> {
    split(command).into_iter().map(strip_wrappers).filter_map(|s| s.words.first().map(|p| p.rsplit('/').next().unwrap_or(p).to_owned())).collect()
}

/// An AppleScript's words outside its strings, lower case.
fn script_words(script: &str) -> Vec<String> {
    let mut words: Vec<String> = vec![];
    let mut quoted = false;
    let mut word = String::new();
    for c in script.chars() {
        if c == '"' { quoted = !quoted; }
        if !quoted && (c.is_alphanumeric() || c == '_') { word.push(c.to_ascii_lowercase()); continue; }
        if !word.is_empty() { words.push(std::mem::take(&mut word)); }
    }
    if !word.is_empty() { words.push(word); }
    words
}
/// Whether a script types or clicks for the user (review 4 Nov: an agent could press its own approval card): a System Events
/// `keystroke` or `key code`, a `click at` a point, or a click, press or keystroke aimed at Heimdall.
fn presses(script: &str) -> bool {
    let words = script_words(script);
    let lower = script.to_lowercase();
    let has = |w: &str| words.iter().any(|x| x == w);
    let pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);
    // Keystrokes and key codes whatever the target is named (`application id "com.apple.systemevents"`).
    has("keystroke") || pair("key", "code") || pair("click", "at")
        || (lower.contains("\"heimdall") && (has("click") || has("perform") || has("select")))
}

/// The part of a delete target the shell cannot change: the path up to the first component that starts with a variable,
/// a substitution or a glob (`x$Y` or `build-*` keep a literal name and count). A guarded `${X:?}` stands for one
/// named folder. Returns the prefix ("." or "/" when nothing is left) and whether a variable cut it.
fn cut(raw: &str) -> (String, bool) {
    let mut kept: Vec<&str> = vec![];
    for (n, comp) in raw.split('/').enumerate() {
        let home = n == 0 && matches!(comp, "$HOME" | "${HOME}" | "~");
        if comp.contains(":?") { kept.push("guarded"); continue; }
        if !home && (comp.starts_with('$') || comp.starts_with('`')) { return (join(&kept, raw), true); }
        if comp.starts_with(['*', '?', '[']) { return (join(&kept, raw), false); }
        kept.push(comp);
    }
    (raw.to_owned(), false)
}
fn join(kept: &[&str], raw: &str) -> String {
    let j = kept.join("/");
    if j.is_empty() { if raw.starts_with('/') { "/".into() } else { ".".into() } } else { j }
}

/// `$X/*`, `"$X"/`, `${X}/*`: an expansion with no `:?` guard right before a slash, as the target's start.
fn unguarded_root(raw: &str) -> bool {
    let w = raw.trim_matches('"');
    if !w.starts_with('$') || w.starts_with("$HOME") || w.starts_with("${HOME}") || w.contains(":?") { return false; }
    let name_end = if let Some(rest) = w.strip_prefix("${") { rest.find('}').map(|i| i + 3) } else { Some(1 + w[1..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(w.len() - 1)) };
    let Some(end) = name_end else { return false };
    let rest = w[end.min(w.len())..].trim_start_matches('"');
    rest == "/" || rest.starts_with("/*") || rest == "/."
}

/// The path with `.` removed and `..` applied to what came before it.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => { out.pop(); }
            c => out.push(c),
        }
    }
    out
}
fn lexical(path: &Path) -> PathBuf { normalize(path) }

/// Its folders through their symlinks, the last part kept as named (`rm` removes a link, not where it leads).
fn resolved(path: &Path) -> PathBuf {
    let Some(name) = path.file_name() else { return path.into() };
    let mut parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&parent) {
            let mut out = real;
            for part in rest.iter().rev() { out.push(part); }
            out.push(name);
            return out;
        }
        match (parent.file_name().map(|n| n.to_owned()), parent.parent().map(Path::to_path_buf)) {
            (Some(n), Some(up)) => { rest.push(n); parent = up; }
            _ => return path.into(),
        }
    }
}

/// Leading `NAME=value`, `env`, `nice -n N`, `nohup`, `time`, `command`, `exec`, `caffeinate`, `xargs` and the like,
/// so the program that runs is judged. (`sudo` stays: it is on the list itself.)
fn strip_wrappers(mut s: Simple) -> Simple {
    loop {
        let Some(first) = s.words.first().cloned() else { return s };
        let drop = if first.contains('=') && !first.starts_with('=') && first.split('=').next().is_some_and(|n| n.chars().all(|c| c.is_alphanumeric() || c == '_')) { 1 }
            else {
                match first.as_str() {
                    "if" | "then" | "else" | "elif" | "do" | "while" | "until" => 1,
                    "command" if s.words.get(1).is_some_and(|w| w == "-v" || w == "-V") => 0,
                    "env" | "nohup" | "time" | "command" | "exec" | "builtin" | "noglob" | "!" => 1 + s.words[1..].iter().take_while(|w| w.starts_with('-') || (first == "env" && w.contains('='))).count(),
                    "nice" | "caffeinate" | "timeout" | "gtimeout" => {
                        let mut n = 1;
                        while let Some(w) = s.words.get(n) {
                            if w.starts_with('-') { n += if matches!(w.as_str(), "-n" | "-t" | "-s" | "-k") { 2 } else { 1 }; }
                            else if first.contains("timeout") && n == 1 { n += 1; break; }
                            else { break; }
                        }
                        n
                    }
                    "xargs" => 1 + s.words[1..].iter().take_while(|w| w.starts_with('-')).count(),
                    _ => 0,
                }
            };
        if drop == 0 || drop > s.words.len() { return s; }
        s.words.drain(..drop);
        s.expands.drain(..drop);
    }
}

/// A shell command line cut into simple commands at `;`, `&`, `|`, new lines, parentheses, braces and `$(` outside
/// quotes. Here-document bodies are skipped unless a shell reads them. Redirect targets are kept apart.
#[allow(unused_assignments)] // the macros' last resets
fn split(command: &str) -> Vec<Simple> {
    let mut out = Vec::new();
    let mut cur = Simple::default();
    let mut word = String::new();
    let mut started = false;
    let mut expands = false;
    let mut redirect = false;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut quote: Option<char> = None;
    macro_rules! end_word { () => {
        if started {
            if redirect { cur.redirects.push(std::mem::take(&mut word)); redirect = false; }
            else { cur.words.push(std::mem::take(&mut word)); cur.expands.push(expands); }
            started = false; expands = false;
        }
    } }
    macro_rules! end_simple { () => { end_word!(); if !cur.words.is_empty() || !cur.redirects.is_empty() { out.push(std::mem::take(&mut cur)); } } }
    while i < chars.len() {
        let c = chars[i];
        match (quote, c) {
            (q, '$') if q != Some('\'') && chars.get(i + 1) == Some(&'(') => {
                // A substitution is its own command line, with its own quotes and here-documents.
                let end = subst_end(&chars, i + 1);
                let inner: String = chars[(i + 2).min(end)..end].iter().collect();
                out.extend(split(&inner));
                word.push_str("$(…)");
                expands = true;
                started = true;
                i = end;
            }
            (None, '$') if chars.get(i + 1) == Some(&'{') => {
                let end = chars[i..].iter().position(|&c| c == '}').map_or(chars.len() - 1, |p| i + p);
                word.extend(&chars[i..=end]);
                expands = true;
                started = true;
                i = end;
            }
            (None, '#') if !started => {
                while i + 1 < chars.len() && chars[i + 1] != '\n' { i += 1; }
            }
            (Some('\''), '\'') => quote = None,
            (Some('\''), c) => word.push(c),
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') if i + 1 < chars.len() => { i += 1; word.push(chars[i]); }
            (Some(_), c) => { if c == '$' || c == '`' { expands = true; } word.push(c); }
            (None, '\\') if i + 1 < chars.len() => { i += 1; if chars[i] != '\n' { word.push(chars[i]); started = true; } }
            (None, '\'' | '"') => { quote = Some(c); started = true; }
            (None, '`') => { end_simple!(); }
            (None, ';' | '|' | '&' | '(' | ')' | '{' | '}') => {
                // `2>&1`, `>&2`: a redirect, not a separator.
                if c == '&' && chars.get(i.wrapping_sub(1)) == Some(&'>') {
                    redirect = false;
                    i += 1;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '-') { i += 1; }
                    continue;
                }
                end_simple!();
            }
            (None, '\n') => {
                end_simple!();
                // Here-document bodies follow the line that opened them.
                for (tag, shell) in std::mem::take(&mut heredocs) {
                    let mut body = String::new();
                    i += 1;
                    while i < chars.len() {
                        let line_end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..line_end].iter().collect();
                        i = line_end + 1;
                        if line.trim() == tag { break; }
                        body.push_str(&line);
                        body.push('\n');
                    }
                    if shell { out.extend(split(&body)); }
                    i -= 1;
                }
            }
            (None, '<') if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                end_word!();
                i += 2;
                if chars.get(i) == Some(&'-') { i += 1; }
                while chars.get(i).is_some_and(|c| *c == ' ') { i += 1; }
                let mut tag = String::new();
                while let Some(&c) = chars.get(i) {
                    if c.is_whitespace() || matches!(c, ';' | '|' | '&' | ')' | '<' | '>') { break; }
                    if c != '\'' && c != '"' && c != '\\' { tag.push(c); }
                    i += 1;
                }
                let program = cur.words.first().map(|p| p.rsplit('/').next().unwrap_or(p).to_owned()).unwrap_or_default();
                let shell = matches!(program.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh") && cur.words.len() == 1;
                heredocs.push((tag, shell));
                continue;
            }
            (None, '>' | '<') => {
                end_word!();
                if c == '>' { redirect = true; while chars.get(i + 1).is_some_and(|n| matches!(n, '>' | '|')) { i += 1; } }
                else { redirect = false; while chars.get(i + 1) == Some(&'<') { i += 1; } }
            }
            (None, c) if c.is_whitespace() => { end_word!(); }
            (None, '$') => { expands = true; word.push(c); started = true; }
            (None, c) => {
                if c.is_ascii_digit() && !started && chars.get(i + 1) == Some(&'>') { i += 1; continue; }
                word.push(c); started = true;
            }
        }
        i += 1;
    }
    end_simple!();
    out
}

/// The index of the `)` closing the substitution whose `(` is at `open`, past quotes, nesting and here-documents.
fn subst_end(chars: &[char], open: usize) -> usize {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut tags: Vec<String> = vec![];
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') | (None, '\\') => i += 1,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => { depth -= 1; if depth == 0 { return i; } }
            (None, '<') if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                i += 2;
                if chars.get(i) == Some(&'-') { i += 1; }
                while chars.get(i) == Some(&' ') { i += 1; }
                let mut tag = String::new();
                while let Some(&t) = chars.get(i) {
                    if t.is_whitespace() || matches!(t, ';' | '|' | '&' | ')' | '<' | '>') { break; }
                    if !matches!(t, '\'' | '"' | '\\') { tag.push(t); }
                    i += 1;
                }
                tags.push(tag);
                continue;
            }
            (None, '\n') => {
                for tag in std::mem::take(&mut tags) {
                    i += 1;
                    while i < chars.len() {
                        let end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = end + 1;
                        if line.trim() == tag { break; }
                    }
                    i -= 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vault() -> PathBuf { PathBuf::from("/Users/t/Work") }
    fn user() -> PathBuf { PathBuf::from("/Users/t") }
    fn bash_in(c: &str, cwd: &str) -> Option<String> { refuse("Bash", &json!({"command": c}), &vault(), &user(), Some(Path::new(cwd))) }
    fn bash(c: &str) -> Option<String> { bash_in(c, "/Users/t/Work/Projects/Heimdall") }

    /// The shapes that stopped tasks on 2 Nov (approvals.log, T3's record) and their kin: all approved.
    #[test]
    fn todays_prompts_and_ordinary_work_pass() {
        for c in [
            "cat >> /Users/t/Work/Assistant/Research/x-evidence.md <<'EOF'\n## Update\nrm -rf / is mentioned here; sudo too\nEOF",
            "du -sh /Users/t/Work/Assistant/Temp/x && rm -rf /Users/t/Work/Assistant/Temp/x && ls /Users/t/Work/Assistant/Temp",
            "pkill -f yt-retry-IGICYFyP_EI.sh; pkill -f \"yt-dlp.*IGICYFyP_EI\"; sleep 2; tail -2 /tmp/x.log",
            "cd /Users/t/Work/Projects/Heimdall && python3 - <<'EOF'\nimport shutil; shutil.rmtree('/')\nEOF",
            "sed -i '' 's/a\\/b/c/' crates/ui/src/mac/controls.rs && nice -n 19 cargo build -j 2",
            "T=$(mktemp -d)/models && mkdir -p $T && cp -R models/x $T/ && rm -rf $(dirname $T)",
            "rm -rf target/debug/incremental build/",
            "cd /Users/t/Work/Assistant/Temp/gpu && rm -rf /Users/t/Work/Assistant/Temp/gpu",
            "rm -rf ~/Library/Developer/Xcode/DerivedData/Arsenal-abc",
            "rm -rf /var/folders/xx/yy/T/heimdall-test",
            "find /Users/t/Work/Projects/Heimdall/target -name '*.o' -delete",
            "rsync -a --delete /tmp/a/ /Users/t/Work/Projects/Site/dist/",
            "echo 'sudo rm -rf / ; diskutil eraseDisk' > /Users/t/Work/Assistant/Temp/notes.txt",
            "git commit -m 'remove launchctl load; sudo note'",
            "security find-identity -v -p codesigning",
            "defaults write com.owner.heimdall-lab Flag -bool true",
            "defaults read com.apple.dock",
            "pmset -g batt",
            "launchctl list | grep heimdall",
            "rm -rf \"${D:?}\"/*",
            "rm -f \"$OUT\"",
            "for f in a b; do rm -rf /Users/t/Work/Assistant/Temp/$f; done",
            "cargo test 2>&1 | tail -20",
            "printf 'x' >> /tmp/heimdall-x/log",
            "ls ~/Library/Keychains-not-really",
            // Review 2 Nov: Claude Code's own commit form, quotes and backticks inside the message.
            "git commit -m \"$(cat <<'EOF'\nHook: User: \"don't ask\" (2 Nov)\n\nOwner's rule: the hook refuses `sudo` now; rm -rf ~ too.\nEOF\n)\"",
            "gh pr create --title x --body \"$(cat <<'EOF'\n## Summary\n- `launchctl load` is refused\nEOF\n)\"",
            "# Let's check\ncargo test\n# User's rule: `sudo` never",
            "sed -n '1,20p' /etc/hosts", "plutil -p ~/Library/Preferences/com.apple.dock.plist",
            "git commit -m \"Drop osascript login item fallback\"", "rg 'login item' | rg osascript", "command -v sudo",
            "defaults write com.apple.dt.Xcode IDEIndexDisable -bool YES",
            "rm -rf /tmp/heimdall-*", "rm -rf /tmp/build-$ID", "cat <<< 'rm -rf ~' | wc -l",
        ] {
            assert_eq!(bash(c), None, "{c}");
        }
        assert_eq!(refuse("Write", &json!({"file_path": "/Users/t/.zshrc"}), &vault(), &user(), None), None, "writes outside the Vault are allowed");
        assert_eq!(refuse("Edit", &json!({"file_path": "/tmp/x/a.rs"}), &vault(), &user(), None), None);
        assert_eq!(refuse("mcp__t3-code__t3_project_delete", &json!({}), &vault(), &user(), None), None);
    }

    #[test]
    fn deletes_outside_the_vault_scratch_and_caches_are_refused() {
        for c in [
            "rm -rf /", "rm -rf ~", "rm -rf ~/", "rm -rf $HOME", "rm -rf /Users/t", "rm -rf /Users/t/Work",
            "rm -rf /Users/t/Work/Projects", "rm -rf ~/Work/Projects/*", "rm -rf /Users/t/Documents/old",
            "rm -rf /Applications/Heimdall.app", "rm -rf /tmp", "rm -rf /tmp/*", "rm -rf ~/.claude",
            "du -sh x && rm -rf /Users/t/Pictures && ls", "nice -n 19 rm -rf /Users/t/Music", "X=1 rm -rf ~/Movies",
            "bash -c 'rm -rf ~/Desktop'", "sh -c \"cd / && rm -rf usr\"", "find ~ -name '*.log' -delete",
            "find /Users/t/Library -exec rm -f {} +", "rsync -a --delete /tmp/empty/ /Users/t/", "rm -rf /tmp/x/../../Users/t",
            "cd ~ && rm -rf *", "cd /Users/t/Work && rm -rf Projects", "unlink /Users/t/.zshrc", "rm -rf \"$D\"/*", "rm -rf $TMP/",
            "if true; then rm -rf /Users/t/Desktop; fi", "rm -rf ${HOME}", "rm -rf ${DIR}/*", "rm -rf ~/$X",
            "rm -rf /Users/t/Documents/$NAME", "rm -rf /tmp/$X", "rm -rf ~/.cache/huggingface/hub/models--x",
            "x=1 <<< hi\nrm -rf ~/Desktop", "git commit -m \"$(echo ok)\" && rm -rf ~/Desktop", "echo $(rm -rf ~/Desktop)", "sudo rm -rf /tmp/x", "xargs rm -rf /Users/t/Desktop",
        ] {
            let r = bash(c);
            assert!(r.as_deref().is_some_and(|m| m.contains("deny list")), "{c}: {r:?}");
        }
        assert!(bash("rm -rf \"$D\"/*").unwrap().contains(":?"), "says how to guard it");
    }

    #[test]
    fn disk_keychain_and_system_settings_are_refused() {
        for (c, word) in [
            ("diskutil eraseDisk APFS X disk4", "disk"), ("diskutil apfs deleteVolume disk3s5", "disk"), ("diskutil partitionDisk disk4 GPT", "disk"),
            ("newfs_apfs /dev/disk4", "disk"), ("dd if=/dev/zero of=/dev/rdisk4 bs=1m", "disk"), ("cat x.img > /dev/disk4", "disk"),
            ("security find-generic-password -s Heimdall -w", "Keychain"), ("security dump-keychain", "Keychain"), ("security delete-generic-password -s x", "Keychain"),
            ("cp ~/Library/Keychains/login.keychain-db /tmp/k", "Keychain"),
            ("sudo pmset -a sleep 0", "system"), ("pmset -b displaysleep 5", "system"), ("defaults write -g AppleInterfaceStyle Dark", "system"),
            ("defaults write com.apple.dock autohide -bool true", "system"), ("defaults delete NSGlobalDomain AppleLanguages", "system"),
            ("launchctl bootout gui/501/com.owner.heimdall", "system"), ("launchctl load ~/Library/LaunchAgents/x.plist", "system"), ("launchctl kickstart -k gui/501/x", "system"),
            ("cp x.plist ~/Library/LaunchAgents/", "system"), ("tee /etc/hosts < x", "system"), ("echo 1 > /etc/sysctl.conf", "system"),
            ("osascript -e 'tell application \"System Events\" to make login item at end with properties {path:\"/A.app\"}'", "system"),
            ("networksetup -setdnsservers Wi-Fi 1.1.1.1", "system"), ("tccutil reset All", "system"), ("csrutil disable", "system"), ("nvram boot-args=-v", "system"),
            ("spctl --master-disable", "system"), ("softwareupdate -ia", "system"), ("scutil --set HostName x", "system"),
            ("cp x ${HOME}/Library/LaunchAgents/", "system"), ("sed -i '' s/a/b/ /etc/hosts", "system"),
        ] {
            let r = bash(c);
            assert!(r.as_deref().is_some_and(|m| m.contains("deny list") && m.to_lowercase().contains(&word.to_lowercase())), "{c}: {r:?}");
        }
        for (tool, path) in [("Write", "/Users/t/Library/LaunchAgents/com.x.plist"), ("Edit", "/etc/hosts"), ("Write", "/Users/t/Library/Preferences/com.apple.dock.plist"), ("Read", "/Users/t/Library/Keychains/login.keychain-db")] {
            assert!(refuse(tool, &json!({"file_path": path}), &vault(), &user(), None).is_some(), "{tool} {path}");
        }
        assert!(refuse("Grep", &json!({"pattern": "x", "path": "/Library/Keychains"}), &vault(), &user(), None).is_some());
    }

    /// Bug note 2026-11-03T18-04-11: listing login items needs no yes; changing them still does, and sfltool's dump is
    /// refused with where to read instead. A move out of protected places into scratch is a delete (the mv-then-rm
    /// workaround); ordinary moves pass.
    #[test]
    fn login_item_reads_pass_and_moves_into_scratch_count_as_deletes() {
        for c in [
            "osascript -e 'tell application \"System Events\" to get the name of every login item'",
            "osascript -e 'tell application \"System Events\" to get {name, path} of every login item'",
            "osascript -e 'tell application \"System Events\"' -e 'set n to (name of every login item)' -e 'end tell'",
            "mv /Users/t/Work/Assistant/Temp/a /tmp/a", "mv ~/Downloads/report.pdf /Users/t/Work/Assistant/Research/",
            "mv /tmp/a /tmp/b", "mv build/x /tmp/x", "mv -f ~/.t3-backup ~/Library/Caches/x", "mv ~/models/x ~/.cache/huggingface/hub/x",
            "osascript -e 'tell application \"System Events\" to get name of every process'",
            "osascript -e 'tell application \"Finder\" to open folder \"x\"'",
        ] {
            assert_eq!(bash(c), None, "{c}");
        }
        for c in [
            "osascript -e 'tell application \"System Events\" to delete login item \"Arsenal\"'",
            "osascript -e 'tell application \"System Events\" to set hidden of login item \"Arsenal\" to true'",
            "osascript -e 'tell application \"System Events\"' -e 'delete (every login item whose name is \"Arsenal\")' -e 'end tell'",
            "sfltool resetbtm", "sfltool dumpbtm",
            "mv ~/Backups/old /tmp/x", "mv /Users/t/Documents /private/tmp/",
            "osascript -e 'tell application \"System Events\" to keystroke \"1\" using option down'",
            "osascript -e 'tell application \"System Events\" to key code 18 using option down'",
            "osascript -e 'tell application \"System Events\" to click at {100, 200}'",
            "osascript -e 'tell application \"System Events\" to click button \"Delete nightly\" of window 1 of process \"Heimdall\"'",
            "cliclick c:100,200",
            // Second review, 4 Nov.
            "osascript -e 'tell application \"System Events\" to tell login item \"Arsenal\" to set hidden to true'",
            "osascript -e 'tell application \"System Events\" to set login item \"Arsenal\"\'s hidden to true'",
            "osascript -l JavaScript -e 'Application(\"System Events\").loginItems.byName(\"Arsenal\").delete()'",
            "osascript -e 'tell application id \"com.apple.systemevents\" to keystroke \"1\" using option down'",
            "mv ~/Backups/old $TMPDIR/x", "mv ~/Backups/old \"${TMPDIR}x\"", "D=$(mktemp -d) && mv ~/Backups/old \"$(mktemp -d)\"",
        ] {
            assert!(bash(c).is_some_and(|m| m.contains("deny list")), "{c}");
        }
        assert!(bash("sfltool dumpbtm").unwrap().contains("System Events"));
        let item = |s: &str| login_item_removal(s);
        assert_eq!(item("tell application \"System Events\" to delete login item \"Arsenal\""), Some("Arsenal".into()));
        assert_eq!(item("TELL APPLICATION \"System Events\"\n  delete every login item whose name is \"Arsenal\"\nend tell"), Some("Arsenal".into()));
        assert_eq!(item("tell application \"System Events\" to delete login item \"A\" & \"B\""), None);
        assert_eq!(item("tell application \"Finder\" to delete login item \"A\""), None);
        assert_eq!(item("tell application \"System Events\" to delete login item \"\u{212a}\""), Some("\u{212a}".into()));
    }

    /// Second review, 4 Nov: a grant covers only a plain rm, rmdir or unlink of one absolute literal path with no symlinked
    /// folder on the way (built in a temporary folder).
    #[test]
    fn only_plain_absolute_literal_deletes_can_be_granted() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("real/x")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let w = |ws: &[&str]| ws.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let real = root.join("real/x");
        let raw = real.display().to_string();
        assert!(grantable_target("rm", &w(&["rm", "-rf", &raw]), &raw, &real));
        let through = root.join("link/x");
        assert!(!grantable_target("rm", &w(&["rm", "-rf"]), &through.display().to_string(), &through), "through a symlinked folder");
        for (name, raw) in [("find", raw.clone()), ("rsync", raw.clone()), ("rm", format!("{raw}/")), ("rm", raw.replace("/real/", "/./real/")),
            ("rm", raw.replace("/real/", "//real/")), ("rm", "~/x".into()), ("rm", "$HOME/x".into()), ("rm", "x".into())] {
            assert!(!grantable_target(name, &w(&[name]), &raw, &real), "{name} {raw}");
        }
        assert!(!grantable_target("rmdir", &w(&["rmdir", "-p", &raw]), &raw, &real), "rmdir -p");
        assert_eq!(programs("du -sh /a && nice -n 19 rm -rf /a; ls | wc -l"), ["du", "rm", "ls", "wc"]);
    }

    /// Review: relative targets are judged from the session's folder and the `cd`s before them; unknown folders pass.
    #[test]
    fn relative_targets_follow_the_working_folder() {
        assert_eq!(bash_in("rm -rf build", "/Users/t/Work/Projects/Site"), None);
        assert!(bash_in("rm -rf Vault", "/Users/t").is_some());
        assert!(bash_in("rm -rf ../../..", "/Users/t/Work/Projects/Site").is_some());
        assert_eq!(refuse("Bash", &json!({"command": "rm -rf build"}), &vault(), &user(), None), None, "no folder known: approved");
    }

    #[test]
    fn a_symlink_out_of_scratch_is_judged_where_it_leads() {
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let scratch = tempfile::tempdir_in(&tmp).unwrap();
        let outside = tempfile::tempdir_in(std::env::var_os("HOME").map(PathBuf::from).unwrap()).unwrap();
        std::fs::create_dir(outside.path().join("work")).unwrap();
        let link = scratch.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let r = |c: String| refuse("Bash", &json!({"command": c}), Path::new("/nonexistent-vault"), &user(), None);
        assert!(r(format!("rm -rf {}/work", link.display())).is_some(), "through the link");
        assert_eq!(r(format!("rm {}", link.display())), None, "the link itself stays in scratch");
        assert_eq!(r(format!("rm -rf {}/own", scratch.path().display())), None);
    }
}
