//! The only things an agent on this Mac may not do without the user ("the agents get the most YOLO mode
//! possible, so they literally don't ask you anything"; docs/harness-autonomy.md). Everything else is approved.
//!
//! The deny list, short on purpose:
//! 1. **Deleting outside the Vault and scratch.** `rm`, `rmdir`, `unlink`, `srm`, `trash`, `find … -delete` / `-exec rm`
//!    (or trash, or a move into scratch), the same fed through `| xargs`, Finder's `delete`, and `rsync --delete` whose
//!    target is not inside the Vault (two folders deep: `<Vault>/Projects/X`, never `Projects`
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
    let ctx = Ctx { vault, user, granted, moved: Default::default(), keychain_used: Default::default(), other_security: Default::default() };
    match tool {
        "Bash" => input["command"].as_str().and_then(|c| ctx.command(c, cwd.map(Path::to_path_buf), 0)).or_else(|| {
            // A command the user's keychain grant let through runs no other `security` call (no find, dump, export or
            // unlock of another keychain beside it).
            (ctx.keychain_used.get() && ctx.other_security.get()).then(|| KEYCHAIN.into())
        }),
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
folders and the build caches. If it really must go, ask User in your final reply, naming each exact path. (Heimdall's \
relay can ask on a card instead: an option with grant.delete naming the exact paths; after his click, delete each with \
one plain rm of its absolute path from /.)";
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

struct Ctx<'a> {
    vault: &'a Path,
    user: &'a Path,
    granted: &'a dyn Fn(&Scope) -> bool,
    /// Where this command line moved something from outside the allowed areas (the destination, and the moved
    /// thing's new path): a later delete of it in the same line is a delete of the original (bug note 2026-11-03T18-04-11).
    moved: std::cell::RefCell<Vec<PathBuf>>,
    /// A `security` call the user's keychain grant covered, and any other `security` call in the same line.
    keychain_used: std::cell::Cell<bool>,
    other_security: std::cell::Cell<bool>,
}

/// One simple command: its words (quotes removed) and whether each word had an expansion (`$`, backticks) outside
/// single quotes.
/// `after_pipe`: it reads the output of the command before it (`a | b`); `via_xargs`: `xargs` ran it, so more operands
/// come from its input.
#[derive(Debug, Default, Clone)]
struct Simple { words: Vec<String>, expands: Vec<bool>, redirects: Vec<String>, after_pipe: bool, via_xargs: bool }

impl Ctx<'_> {
    fn command(&self, command: &str, mut cwd: Option<PathBuf>, depth: usize) -> Option<String> {
        if depth > 4 { return None; }
        let mut last: Option<Simple> = None;
        for simple in split(command) {
            let simple = strip_wrappers(simple);
            let prev = std::mem::replace(&mut last, Some(simple.clone()));
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
            if name == "mv" {
                if let Some(r) = self.moves_to_scratch(args, cwd.as_deref(), command.contains("mktemp")) { return Some(r); }
                // `… | xargs mv {} ~/.Trash/`: what it moves comes from its input; into the Trash or a temporary folder
                // that is a delete of each.
                if simple.via_xargs && self.moves_into_scratch(args, cwd.as_deref()) {
                    let fed: Vec<(String, bool)> = fed(&simple, prev.as_ref()).into_iter().map(|w| (w, false)).collect();
                    if let Some(r) = self.judge_deletes("mv", &simple.words, &fed, cwd.as_deref()) { return Some(r); }
                }
            }
            if name == "osascript" { if let Some(r) = self.finder_deletes(&osascript_script(args), cwd.as_deref()) { return Some(r); } }
            if name == "security" {
                if self.keychain_grant(&simple) { self.keychain_used.set(true); continue; }
                self.other_security.set(true);
            }
            if let Some(r) = self.disk(&name, args).or_else(|| self.security(&name, args)).or_else(|| self.system(&name, args)) {
                return Some(r);
            }
            if let Some(r) = self.deletes(&name, &simple, cwd.as_deref(), prev.as_ref()) { return Some(r); }
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
        // Only -q, -v and -h may come before the verb: -i reads commands from its input and -p prompts for them
        // (`echo delete-keychain … | security -i` would pass as `help`).
        let global: Vec<&String> = args.iter().take_while(|a| a.starts_with('-')).collect();
        if global.iter().any(|g| !g[1..].chars().all(|c| matches!(c, 'q' | 'v' | 'h')) || g.len() < 2) { return Some(KEYCHAIN.into()); }
        let verb = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("help");
        // `list-keychains -s` sets the search list.
        let sets = verb == "list-keychains" && args.iter().any(|a| a == "-s" || a.starts_with("-s"));
        (!READ_ONLY.contains(&verb) || sets).then(|| KEYCHAIN.into())
    }

    /// Whether one `security` call is covered by the user's keychain grant (bug note 2026-11-04T23-01-22): the verb is
    /// create-keychain, set-keychain-settings, unlock-keychain, import or set-key-partition-list (or the read-only
    /// find-identity and show-keychain-info), no global option comes before it, every option is one that verb
    /// knows and none prompts on the user's screen (`-P`), and the call names exactly one keychain: the granted file,
    /// written as one absolute literal path from `/` with no symlinked folder on the way (import's `-k`, the others'
    /// last word). create-keychain needs the file not to exist yet; the grant itself needs it new since the click.
    fn keychain_grant(&self, s: &Simple) -> bool {
        let (words, expands) = (&s.words, &s.expands);
        let Some(verb) = words.get(1) else { return false };
        // Per verb: options that take no value, options that take one, options that take two.
        let (flags, one, two): (&[&str], &[&str], &[&str]) = match verb.as_str() {
            "create-keychain" | "unlock-keychain" => (&[], &["-p"], &[]),
            "set-keychain-settings" => (&["-l", "-u", "-lu", "-ul"], &["-t"], &[]),
            "import" => (&["-w", "-x", "-A"], &["-k", "-t", "-f", "-T"], &["-a"]),
            "set-key-partition-list" => (&["-s"], &["-S", "-k", "-D", "-a", "-c", "-j", "-l", "-t"], &[]),
            "find-identity" => (&["-v"], &["-p", "-s"], &[]),
            "show-keychain-info" => (&[], &[], &[]),
            _ => return false,
        };
        let mut positional: Vec<usize> = vec![];
        let mut keychain_option: Vec<usize> = vec![];
        let mut i = 2;
        while i < words.len() {
            let w = &words[i];
            if w.starts_with('-') && w.len() > 1 {
                // `security` stops reading options at its first other word (getopt), so an option after it would be
                // read as one more keychain name (review 5 Nov): refused. import alone takes its file first, then options.
                if !positional.is_empty() && !(verb == "import" && positional == [2]) { return false; }
                if flags.contains(&w.as_str()) { i += 1; }
                else if one.contains(&w.as_str()) {
                    if w == "-k" && verb == "import" { keychain_option.push(i + 1); }
                    i += 2;
                } else if two.contains(&w.as_str()) { i += 3; }
                else { return false; }
                if i > words.len() { return false; }
            } else { positional.push(i); i += 1; }
        }
        let keychain_at = if verb == "import" {
            // The file imported (one, never a keychain) and `-k` the granted keychain, once.
            // The file is the first word after the verb, before any option.
            let [input] = positional[..] else { return false };
            if input != 2 { return false; }
            let raw = &words[input];
            if expands[input] || raw.ends_with(".keychain") || raw.ends_with(".keychain-db") { return false; }
            if self.absolute(raw, None).is_some_and(|p| self.keychain(&p).is_some()) { return false; }
            let [k] = keychain_option[..] else { return false };
            k
        } else {
            let [k] = positional[..] else { return false };
            k
        };
        let Some(raw) = words.get(keychain_at) else { return false };
        if expands[keychain_at] || !raw.starts_with('/') || !exact(raw) || raw[1..].split('/').any(|c| c.is_empty() || c == "." || c == "..") { return false; }
        let path = PathBuf::from(raw);
        if resolved(&path) != path { return false; }
        if verb == "create-keychain" && std::fs::symlink_metadata(&path).is_ok() { return false; }
        (self.granted)(&Scope::Keychain(path))
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

    /// A delete's targets, each judged by `deletable`. `prev`: the command before it in the line, which feeds it when it
    /// reads a pipe through `xargs`.
    fn deletes(&self, name: &str, simple: &Simple, cwd: Option<&Path>, prev: Option<&Simple>) -> Option<String> {
        let words = &simple.words;
        // Each target as written, and whether the user's grant may cover it (only a word of this command, never one an
        // expansion spelled).
        let own = |i: usize| (words[i].clone(), !simple.expands.get(i).copied().unwrap_or(true));
        let mut targets: Vec<(String, bool)> = match name {
            // `trash` moves into the Trash: a delete (second review 5 Nov; /usr/bin/trash ships with macOS).
            n if DELETERS.contains(&n) => {
                let mut options = true;
                (1..words.len()).filter(|&i| {
                    let w = &words[i];
                    if options && w == "--" { options = false; return false; }
                    !(options && w.starts_with('-') && w.len() > 1)
                }).map(own).collect()
            }
            "find" if self.find_deletes(words, cwd) => find_starts(words).into_iter().map(|w| (w, false)).collect(),
            "rsync" if words.iter().any(|w| w.starts_with("--delete") || w == "--remove-source-files") => {
                words.iter().rposition(|w| !w.starts_with('-')).filter(|&i| i > 0).into_iter().map(own).collect()
            }
            _ => return None,
        };
        // `… | xargs rm`: the paths come from the command before it (`fed`).
        if simple.via_xargs { targets.extend(fed(simple, prev).into_iter().map(|w| (w, false))); }
        self.judge_deletes(name, words, &targets, cwd)
    }

    /// Each delete target judged by `deletable`; a refused one passes only on the user's grant for exactly that path.
    fn judge_deletes(&self, name: &str, words: &[String], targets: &[(String, bool)], cwd: Option<&Path>) -> Option<String> {
        for (raw, may_grant) in targets {
            if self.deletes_moved(raw, self.absolute(&cut(raw).0, cwd).as_deref()) { return Some(DELETE.into()); }
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
            if *may_grant && grantable_target(name, words, raw, &path) && prefix == *raw && (self.granted)(&Scope::Delete(path)) { continue; }
            return Some(refusal.into());
        }
        None
    }

    /// Whether a `find` deletes what it finds: `-delete`, or an `-exec`/`-execdir`/`-ok`/`-okdir` command that deletes
    /// (`rm`, `rmdir`, `unlink`, `srm`, `trash`, a move into a temporary folder or the Trash, or a shell's `-c` script
    /// doing one of those).
    fn find_deletes(&self, words: &[String], cwd: Option<&Path>) -> bool {
        if words.iter().any(|w| w == "-delete") { return true; }
        let mut i = 0;
        while i < words.len() {
            if matches!(words[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                let end = words[i + 1..].iter().position(|w| w == ";" || w == "+").map_or(words.len(), |p| i + 1 + p);
                let exec = Simple { words: words[i + 1..end].to_vec(), expands: vec![false; end - i - 1], ..Default::default() };
                if self.exec_deletes(strip_wrappers(exec), cwd, 0) { return true; }
                i = end;
            }
            i += 1;
        }
        false
    }

    /// Whether one command `find -exec` or `xargs` runs on each path deletes it.
    fn exec_deletes(&self, exec: Simple, cwd: Option<&Path>, depth: usize) -> bool {
        let Some(program) = exec.words.first() else { return false };
        let name = program.rsplit('/').next().unwrap_or(program);
        if DELETERS.contains(&name) { return true; }
        if name == "mv" { return self.moves_into_scratch(&exec.words[1..], cwd); }
        if matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh") && depth < 2 {
            let script = exec.words.iter().position(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))).and_then(|i| exec.words.get(i + 1));
            return script.is_some_and(|s| split(s).into_iter().map(strip_wrappers).any(|e| self.exec_deletes(e, cwd, depth + 1)));
        }
        false
    }

    /// Whether `mv`'s destination (its last operand) is a temporary folder or the Trash.
    fn moves_into_scratch(&self, args: &[String], cwd: Option<&Path>) -> bool {
        let operands: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        operands.last().is_some_and(|d| self.scratch(d, cwd, false))
    }

    /// Whether `dest_word` is in a temporary folder or the Trash. `mktemp`: the command makes a temporary folder, so a
    /// substitution as the destination is one.
    fn scratch(&self, dest_word: &str, cwd: Option<&Path>, mktemp: bool) -> bool {
        let temp_word = dest_word.starts_with("$TMPDIR") || dest_word.starts_with("${TMPDIR") || (mktemp && dest_word.starts_with("$("));
        let dest = if temp_word { Some(PathBuf::from("/tmp/x")) } else { self.absolute(dest_word, cwd) };
        // Only the temporary folders and the Trash: a move into a build cache (`~/.cache/huggingface`) is ordinary work.
        let mut temps: Vec<PathBuf> = ["/tmp", "/private/tmp", "/var/folders", "/private/var/folders"].iter().map(PathBuf::from).collect();
        if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) { temps.push(lexical(Path::new(&t))); }
        temps.push(self.user.join(".Trash"));
        // Compared without case, as APFS does (`~/.TRASH`, `/TMP`).
        let fold = |p: &Path| PathBuf::from(lexical(p).to_string_lossy().to_lowercase());
        dest.as_ref().is_some_and(|d| temps.iter().any(|t| fold(d).starts_with(fold(t))))
    }

    /// A Finder script that deletes, moves to the Trash or empties it (`tell application "Finder" to delete POSIX file
    /// "/x"`): allowed only when every path it names is a literal absolute path inside the allowed areas and it names
    /// nothing else (no `home`, `folder`, `every …`, a variable); emptying the Trash is always a delete.
    fn finder_deletes(&self, script: &str, cwd: Option<&Path>) -> Option<String> {
        let words = script_words(script);
        let has = |w: &str| words.iter().any(|x| x == w);
        // The application's name is a string (`application "Finder"`), JXA's too (`Application("Finder")`).
        if !script.to_lowercase().contains("finder") || !(has("delete") || has("trash") || has("erase")) { return None; }
        if has("empty") || has("erase") { return Some(DELETE.into()); }
        let vague = ["home", "desktop", "folder", "disk", "every", "whose", "items", "item", "selection", "startup", "set", "repeat", "my", "of", "it"];
        if words.iter().any(|w| vague.contains(&w.as_str())) || script.contains('$') { return Some(DELETE.into()); }
        let paths: Vec<String> = script.split('"').skip(1).step_by(2).filter(|q| !q.eq_ignore_ascii_case("finder")).map(str::to_owned).collect();
        if paths.is_empty() || paths.iter().any(|p| !p.starts_with('/')) { return Some(DELETE.into()); }
        let targets: Vec<(String, bool)> = paths.into_iter().map(|p| (p, false)).collect();
        self.judge_deletes("osascript", &[], &targets, cwd)
    }

    /// `mv` of something outside the allowed areas into a temporary folder or the Trash: a delete in two steps. Into
    /// anywhere else it is ordinary work, but the destination is remembered (`moved`), so deleting it later in the same
    /// command line is refused like deleting the original (bug note 2026-11-03T18-04-11: mv, then rm).
    /// `mktemp`: the command makes a temporary folder, so a substitution as the destination is one.
    fn moves_to_scratch(&self, args: &[String], cwd: Option<&Path>, mktemp: bool) -> Option<String> {
        let operands: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        let (dest_word, sources) = operands.split_last()?;
        let temp_word = dest_word.starts_with("$TMPDIR") || dest_word.starts_with("${TMPDIR") || (mktemp && dest_word.starts_with("$("));
        let dest = if temp_word { Some(PathBuf::from("/tmp/x")) } else { self.absolute(dest_word, cwd) };
        let scratch = self.scratch(dest_word, cwd, mktemp);
        for s in sources {
            let (prefix, _) = cut(s);
            let Some(path) = self.absolute(&prefix, cwd) else { continue };
            // Moved here from outside earlier in the line, it is still the original.
            if self.deletable(&path) && !self.deletes_moved(s, Some(&path)) { continue; }
            if scratch { return Some(DELETE.into()); }
            // An unknowable destination (`mv ~/x "$D"`) cannot be followed: refuse a later delete of any variable.
            let Some(dest) = &dest else { self.moved.borrow_mut().push(PathBuf::from("$")); continue };
            // Into a folder (one that exists, or written with a trailing slash) it keeps its name; else it is renamed.
            let into = dest_word.ends_with('/') || dest.is_dir() || sources.len() > 1;
            let landed = match path.file_name() { Some(name) if into => dest.join(name), _ => dest.clone() };
            self.moved.borrow_mut().push(landed);
        }
        None
    }

    /// Whether a delete target in this command line reaches something it moved here from outside the allowed areas: the
    /// moved thing, inside it, or a folder holding it.
    fn deletes_moved(&self, raw: &str, path: Option<&Path>) -> bool {
        let moved = self.moved.borrow();
        if moved.is_empty() { return false; }
        if raw.contains('$') && moved.iter().any(|m| m.as_os_str() == "$") { return true; }
        let Some(path) = path.map(lexical) else { return false };
        moved.iter().any(|m| m.starts_with(&path) || path.starts_with(m))
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

/// Programs that delete their operands. `trash` moves them into the Trash, which counts as a delete.
const DELETERS: [&str; 5] = ["rm", "rmdir", "unlink", "srm", "trash"];

/// A `find`'s start points: the words after its own options and before the first expression.
fn find_starts(words: &[String]) -> Vec<String> {
    (1..words.len()).skip_while(|&i| matches!(words[i].as_str(), "-H" | "-L" | "-P" | "-E" | "-X" | "-d" | "-s" | "-x"))
        .take_while(|&i| !words[i].starts_with('-') && !matches!(words[i].as_str(), "(" | "!")).map(|i| words[i].clone()).collect()
}

/// The paths `xargs` hands a command from its input: a piped `find`'s start points (or `.` when it has none), the
/// words a piped `echo` or `printf` prints, the folders a piped `ls` lists (`.` when none), the working folder for
/// `git`'s file lists; else the home folder, which refuses the delete.
/// Review 5 Nov: anything else fed in went as the working folder, so `mdfind -name x | xargs rm`, `cat list | xargs
/// rm` and `echo ~/x | tr … | xargs trash` passed from the Vault. Input that cannot be known is judged as home.
fn fed(simple: &Simple, prev: Option<&Simple>) -> Vec<String> {
    let program = |p: &Simple| p.words.first().map(|w| w.rsplit('/').next().unwrap_or(w).to_owned()).unwrap_or_default();
    let operands = |p: &Simple| p.words[1..].iter().filter(|w| !w.starts_with('-')).cloned().collect::<Vec<_>>();
    match prev.filter(|_| simple.after_pipe) {
        Some(p) if program(p) == "find" => {
            let starts = find_starts(&p.words);
            if starts.is_empty() { vec![".".into()] } else { starts }
        }
        // `echo /x | xargs rm`: the words it prints, and the working folder for anything else in them.
        Some(p) if matches!(program(p).as_str(), "echo" | "printf") => operands(p).into_iter().chain([".".to_owned()]).collect(),
        // `ls | xargs trash` lists the working folder; `ls -d ~/x | xargs rm` the folders it names.
        Some(p) if program(p) == "ls" => {
            let named = operands(p);
            if named.is_empty() { vec![".".into()] } else { named }
        }
        // `git ls-files -d | xargs rm`, `git diff --name-only`: paths inside the repository, relative to the working
        // folder. Other git commands print absolute paths (`rev-parse --git-common-dir`, `worktree list`): unknown.
        Some(p) if program(p) == "git" && git_lists_relative_files(&p.words) => vec![".".into()],
        _ => vec!["~".into()],
    }
}

/// Whether a piped `git` prints file paths relative to the working folder: `ls-files`, or `diff`/`show`/`log` with
/// `--name-only`, `status` with `--porcelain`/`--short`, without `-C`, `--git-dir`, `--work-tree` or an absolute word.
fn git_lists_relative_files(words: &[String]) -> bool {
    let moved = words.iter().any(|w| w == "-C" || w.starts_with("--git-dir") || w.starts_with("--work-tree") || w.starts_with('/') || w.starts_with('~') || w.starts_with('$'));
    let has = |f: &str| words.iter().any(|w| w == f);
    let sub = words.iter().skip(1).find(|w| !w.starts_with('-')).map(String::as_str);
    !moved && match sub {
        Some("ls-files") => true,
        Some("diff" | "show" | "log") => has("--name-only"),
        Some("status") => has("--porcelain") || has("--short") || has("-s"),
        _ => false,
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

/// Whether a refused delete target can be one of the user's grants (review 4 Nov): only a plain `rm`, `unlink`, `rmdir` or `trash`
/// (never `rmdir -p`, `find` or `rsync`, which reach past it), written as one absolute literal path from `/` (never
/// `~`, `$HOME`, the working folder, a glob or a variable), with no `.`, `..` or empty component, no trailing slash
/// (through a symlink it deletes the target) and no symlinked folder on the way.
fn grantable_target(name: &str, words: &[String], raw: &str, path: &Path) -> bool {
    let parents = name == "rmdir" && words.iter().any(|w| w == "--parents" || (w.starts_with('-') && !w.starts_with("--") && w.contains('p')));
    matches!(name, "rm" | "rmdir" | "unlink" | "trash") && !parents
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
                    "xargs" => {
                        // Options, and the value of those that take one as the next word (`xargs -I {} mv {} …`).
                        let mut n = 1;
                        while let Some(w) = s.words.get(n).filter(|w| w.starts_with('-')) {
                            n += if matches!(w.as_str(), "-I" | "-J" | "-L" | "-n" | "-P" | "-s" | "-E" | "-R" | "-S" | "-a" | "-d") { 2 } else { 1 };
                        }
                        n
                    }
                    _ => 0,
                }
            };
        if drop == 0 || drop > s.words.len() { return s; }
        if first == "xargs" { s.via_xargs = true; }
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
            (None, '$') if chars.get(i + 1) == Some(&'\'') => {
                // ANSI-C quoting (`$'…'`): a backslash escapes, `\'` included, so the string ends only at an unescaped
                // quote (review 5 Nov: `echo $'\''; security dump-keychain; echo $'\''` hid the middle command). Its
                // escapes are decoded as the shell does (`ansi_c`, second review 5 Nov: `$'\057Users/t/Documents'` read
                // as a relative path and `$'\x72m'` was not rm). They can spell any character, so the word counts as an
                // expansion: never a grantable literal.
                i = ansi_c(&chars, i + 2, &mut word);
                expands = true;
                started = true;
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
            // `{` and `}` group commands only as words of their own; `{}` (find's and xargs' placeholder) and `a{b,c}`
            // are words.
            (None, '{' | '}') if started || !chars.get(i + 1).is_none_or(|n| n.is_whitespace() || matches!(n, ';' | '&' | '|' | ')')) => {
                word.push(c);
                started = true;
            }
            (None, ';' | '|' | '&' | '(' | ')' | '{' | '}') => {
                // `2>&1`, `>&2`: a redirect, not a separator.
                if c == '&' && chars.get(i.wrapping_sub(1)) == Some(&'>') {
                    redirect = false;
                    i += 1;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '-') { i += 1; }
                    continue;
                }
                end_simple!();
                // `a | b` (not `a || b`): b reads a's output.
                cur.after_pipe = c == '|' && chars.get(i + 1) != Some(&'|') && chars.get(i.wrapping_sub(1)) != Some(&'|');
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

/// Decodes the body of an ANSI-C quoted string (`$'…'`) starting at `start` into `word`, as bash does: `\a \b \e \E \f \n \r
/// \t \v \\ \' \" \?`, octal `\nnn`, `\xHH`, `\uHHHH`, `\UHHHHHHHH` and control `\cX`; any other backslash stays. A NUL ends
/// the string there (the shell's C string stops at it; the rest up to the quote is dropped). Returns the index of
/// the closing quote (or the end).
fn ansi_c(chars: &[char], start: usize, word: &mut String) -> usize {
    let mut i = start;
    let mut ended = false;
    let digits = |i: usize, max: usize, radix: u32| -> (u32, usize) {
        let mut value = 0u32;
        let mut n = 0;
        while n < max && chars.get(i + n).is_some_and(|c| c.is_digit(radix)) {
            value = value.saturating_mul(radix).saturating_add(chars[i + n].to_digit(radix).unwrap());
            n += 1;
        }
        (value, n)
    };
    while i < chars.len() && chars[i] != '\'' {
        let mut out: Option<char> = Some(chars[i]);
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            let e = chars[i];
            out = match e {
                'a' => Some('\u{7}'), 'b' => Some('\u{8}'), 'e' | 'E' => Some('\u{1b}'), 'f' => Some('\u{c}'), 'n' => Some('\n'),
                'r' => Some('\r'), 't' => Some('\t'), 'v' => Some('\u{b}'), '\\' | '\'' | '"' | '?' => Some(e),
                '0'..='7' => {
                    let (v, n) = digits(i, 3, 8);
                    i += n - 1;
                    Some(char::from((v & 0xff) as u8))
                }
                'x' | 'u' | 'U' => {
                    let (v, n) = digits(i + 1, match e { 'x' => 2, 'u' => 4, _ => 8 }, 16);
                    if n == 0 { word.push('\\'); Some(e) }
                    else {
                        i += n;
                        // An invalid code point is not a character the program could be named with: kept visible.
                        Some(if e == 'x' { char::from(v as u8) } else { char::from_u32(v).unwrap_or('\u{fffd}') })
                    }
                }
                'c' if i + 1 < chars.len() => {
                    i += 1;
                    let c = chars[i];
                    if c.is_ascii() { Some(char::from((c as u8) & 0x1f)) } else { word.push_str("\\c"); Some(c) }
                }
                _ => { word.push('\\'); Some(e) }
            };
        }
        if out == Some('\0') { ended = true; }
        if !ended { if let Some(c) = out { word.push(c); } }
        i += 1;
    }
    i
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
            (None, '$') if chars.get(i + 1) == Some(&'\'') => {
                i += 2;
                while i < chars.len() && chars[i] != '\'' { if chars[i] == '\\' { i += 1; } i += 1; }
            }
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

    /// Bug note 2026-11-03T18-04-11, second half: moving something out of a protected folder and deleting it in the
    /// same line is a delete of the original; a move into the Trash is a delete. Ordinary moves and deletes pass.
    #[test]
    fn moving_out_then_deleting_in_one_line_is_a_delete() {
        for c in [
            "mv ~/Backups/old ~/.Trash/", "mv /Users/t/Documents/x /Users/t/.Trash/x",
            "mv ~/Backups/old /Users/t/Work/Assistant/Temp/old && rm -rf /Users/t/Work/Assistant/Temp/old",
            "mv ~/Backups/old /Users/t/Work/Assistant/Temp/ && rm -rf /Users/t/Work/Assistant/Temp/old",
            "mv ~/Backups/old /Users/t/Work/Projects/X/old; rm -rf /Users/t/Work/Projects/X",
            "mv ~/Backups/old /Users/t/Work/Projects/X/old && find /Users/t/Work/Projects/X/old -delete",
            "mv ~/Backups/old \"$D\" && rm -rf \"$D\"",
            "cd /Users/t/Work/Assistant/Temp && mv ~/Backups/old gone && rm -rf gone",
            // Review 5 Nov: a second move of the moved thing into the Trash, and the Trash or /tmp in other case.
            "mv ~/Backups/old /Users/t/Work/Assistant/Temp/old && mv /Users/t/Work/Assistant/Temp/old ~/.Trash/",
            "mv ~/Documents/x ~/.TRASH/", "mv ~/Documents/x /TMP/x",
        ] {
            assert!(bash(c).is_some_and(|m| m.contains("deletes outside")), "{c}");
        }
        for c in [
            "mv ~/Downloads/report.pdf /Users/t/Work/Assistant/Research/ && rm -rf /Users/t/Work/Assistant/Temp/scratch",
            "mv /Users/t/Work/Assistant/Temp/a /Users/t/Work/Assistant/Temp/b && rm -rf /Users/t/Work/Assistant/Temp/b",
            "mv ~/Downloads/x.zip /Users/t/Work/Projects/X/x.zip && rm -rf /Users/t/Work/Projects/X/build",
            "mv /Users/t/Work/Projects/X/a ~/.Trash/",
        ] {
            assert_eq!(bash(c), None, "{c}");
        }
    }

    /// The Keychain guard reads `security`'s own options: -i and -p before the verb run commands from input, and
    /// `list-keychains -s` sets the search list.
    #[test]
    fn security_interactive_mode_and_search_list_changes_are_refused() {
        for c in ["security -i", "echo 'delete-keychain login.keychain' | security -i", "security -p 'k> '", "security -iq",
            "security list-keychains -s ~/Library/Keychains/x.keychain-db", "security list-keychains -d user -s /tmp/x.keychain-db",
            "security default-keychain -s /tmp/x.keychain-db", "security -q dump-keychain"] {
            assert!(bash(c).is_some_and(|m| m.contains("Keychain")), "{c}");
        }
        for c in ["security list-keychains", "security list-keychains -d user", "security -v find-identity -p codesigning", "security -h"] {
            assert_eq!(bash(c), None, "{c}");
        }
        // Review 5 Nov: ANSI-C quotes end only at an unescaped quote, so a command between two of them is still seen.
        for c in ["echo $'\\''; security dump-keychain -d; echo $'\\''", "echo $'\\''; rm -rf ~/Documents; echo $'\\''",
            "X=$(echo $'\\)'; security find-generic-password -s x -w)"] {
            assert!(bash(c).is_some_and(|m| m.contains("deny list")), "{c}");
        }
        assert_eq!(bash("printf $'a\\tb\\n' && echo $'it\\'s fine' | wc -c"), None);
    }

    /// Bug note 2026-11-04T23-01-22: the user's grant for one new keychain file lets exactly the signing setup on that file
    /// through, and nothing else in the Keychain: not the login or default keychain, another file, a variable, a
    /// prompt, a read or unlock of another keychain beside it, or a file that already exists.
    #[test]
    fn a_keychain_grant_covers_its_one_new_file_only() {
        let k = "/Users/t/Library/Keychains/heimdall-release-signing.keychain-db";
        let granted = |s: &Scope| *s == Scope::Keychain(PathBuf::from(k));
        let r = |c: &str| refuse_with("Bash", &json!({"command": c}), &vault(), &user(), Some(Path::new("/Users/t/Work/Projects/P/keys")), &granted);
        let setup = format!("security create-keychain -p \"$(cat keychain-password)\" {k} && security set-keychain-settings {k} && \
            security unlock-keychain -p \"$(cat keychain-password)\" {k} && security import key.pem -k {k} -T /usr/bin/codesign >/dev/null && \
            security import cert.pem -k {k} >/dev/null && security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k \"$(cat keychain-password)\" {k} >/dev/null && \
            security find-identity -p codesigning {k}");
        assert_eq!(r(&setup), None, "the whole setup on the granted file");
        assert!(bash(&setup).is_some(), "no grant, no keychain");
        let login = "/Users/t/Library/Keychains/login.keychain-db";
        for c in [
            format!("security import key.pem -k {login}"), "security import key.pem".into(), format!("security import key.pem -k {k} -k {login}"),
            "security unlock-keychain -p x".into(), format!("security unlock-keychain -p x {login}"), "security set-key-partition-list -S apple: -s -k pw".into(),
            "security set-keychain-settings".into(), "security create-keychain -p x /Users/t/Library/Keychains/other.keychain-db".into(),
            // Review 5 Nov: `security` reads no option after its first other word, so these would name more keychains.
            format!("security set-key-partition-list -S apple: -s -k pw {k} -l login.keychain-db"), format!("security import -x key.pem -k {k}"),
            format!("security unlock-keychain {k} -p x"),
            "security import key.pem -k \"$KC\"".into(), "security import key.pem -k $HOME/Library/Keychains/heimdall-release-signing.keychain-db".into(),
            "security import key.pem -k ~/Library/Keychains/heimdall-release-signing.keychain-db".into(),
            format!("security create-keychain -P {k}"), format!("security import key.pem -P -k {k}"), format!("security -q create-keychain -p x {k}"),
            format!("security create-keychain -p x {k} {login}"), format!("security import {login} -k {k}"), format!("security import ~/Library/Keychains/login.keychain-db -k {k}"),
            format!("security create-keychain -p x {k} && security unlock-keychain -p y {login}"),
            format!("security create-keychain -p x {k}; security find-generic-password -s Heimdall -w"),
            format!("security create-keychain -p x {k}; security find-identity -v"), format!("security create-keychain -p x {k}; security find-certificate -a -p"),
            format!("security create-keychain -p x {k}; security dump-keychain {k}"), format!("security export -k {k} -o /tmp/x.p12"),
            format!("security delete-keychain {k}"), format!("security default-keychain -s {k}"), format!("security list-keychains -s {k}"),
            format!("security create-keychain -p x {k}; security -i"), format!("security create-keychain -p x {k} && cp {login} /tmp/x"),
            format!("security set-key-partition-list -S apple: -s -k pw -X {k}"),
        ] {
            assert!(r(&c).is_some_and(|m| m.contains("Keychain")), "{c}");
        }
        // A file that exists already is never made again, and a symlinked folder on the way is refused.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let old = root.join("old.keychain-db");
        std::fs::write(&old, b"x").unwrap();
        std::os::unix::fs::symlink(&root, root.join("link")).unwrap();
        let through = root.join("link/new.keychain-db");
        let any = |_: &Scope| true;
        let r2 = |c: String| refuse_with("Bash", &json!({"command": c}), &vault(), &user(), None, &any);
        assert!(r2(format!("security create-keychain -p x {}", old.display())).is_some(), "exists already");
        assert!(r2(format!("security create-keychain -p x {}", through.display())).is_some(), "through a symlinked folder");
        assert_eq!(r2(format!("security create-keychain -p x {}", root.join("new.keychain-db").display())), None);
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

    /// Second review, 5 Nov (HIGH): `trash` and `find … -exec mv {} ~/.Trash \;` went round the "moving into the Trash
    /// is a delete" rule. `trash`, every `find` that deletes or trashes what it finds (`-delete`, `-exec`/`-execdir`/
    /// `-ok` with rm, trash or a move into scratch, also inside `sh -c`), `xargs` fed from a pipe and Finder's delete are
    /// judged like `rm`: inside the Vault, scratch and caches they pass, elsewhere they are refused unless the user granted
    /// exactly that path.
    #[test]
    fn trash_find_exec_xargs_and_finder_deletes_follow_the_delete_rules() {
        for c in [
            "trash ~/Documents/old", "trash /Users/t/Desktop/x.png", "/usr/bin/trash -v ~/Backups", "nice -n 19 trash ~/Music",
            "trash -- ~/Movies", "cd ~ && trash Documents",
            "find ~/Documents -name '*.bak' -exec mv {} ~/.Trash \\;", "find ~/Documents -exec mv {} /tmp/ +",
            "find ~/Documents -execdir trash {} \\;", "find ~/Documents -ok rm {} \\;", "find ~/Documents -exec /bin/rm -f {} +",
            "find ~/Documents -exec sh -c 'mv \"$1\" ~/.Trash/' _ {} \\;", "find ~/Documents -exec nice -n 19 trash {} +",
            "find ~/Documents -name x | xargs trash", "find ~/Documents | xargs rm -rf", "find ~/Documents -print0 | xargs -0 -I {} mv {} ~/.Trash/",
            "find ~/Documents | xargs -I{} mv {} /tmp/", "find ~/Documents -exec trash {} \\; ; echo done", "{ trash ~/Documents; }",
            "osascript -e 'tell application \"Finder\" to delete POSIX file \"/Users/t/Documents/old\"'",
            "osascript -e 'tell application \"Finder\" to delete (POSIX file \"/Users/t/Work/Projects/X/a\") & file \"b\" of home'",
            "osascript -e 'tell application \"Finder\" to delete every item of folder \"Documents\" of home'",
            "osascript -e 'tell application \"Finder\" to empty trash'",
            "osascript -e 'tell application \"Finder\" to move POSIX file \"/Users/t/Documents/x\" to trash'",
            "osascript -l JavaScript -e 'Application(\"Finder\").delete(Path(\"/Users/t/Documents/x\"))'",
        ] {
            assert!(bash_in(c, "/Users/t/Work/Projects/Heimdall").is_some_and(|m| m.contains("deny list")), "{c}");
        }
        assert!(bash_in("ls | xargs trash", "/Users/t").is_some(), "fed from the home folder");
        // Review 5 Nov: input from any other program, or a list file, is not known to stay in the working folder.
        for c in [
            "mdfind -name '*.dmg' | xargs rm", "ls -d ~/Documents/old | xargs rm -rf", "echo ~/x | tr ' ' '\\n' | xargs trash",
            "cat list.txt | xargs rm", "xargs rm < list.txt", "git -C ~/Documents ls-files | xargs rm",
            "git rev-parse --git-common-dir | xargs rm -rf", "git rev-parse --show-toplevel | xargs rm -rf",
            "git worktree list --porcelain | xargs rm -rf",
        ] {
            assert!(bash_in(c, "/Users/t/Work/Projects/Heimdall").is_some_and(|m| m.contains("deny list")), "{c}");
        }
        for c in ["ls build | xargs rm", "git diff --name-only | xargs rm", "git status --porcelain | xargs rm -f"] {
            assert_eq!(bash_in(c, "/Users/t/Work/Projects/Heimdall"), None, "{c}");
        }
        for c in [
            "trash /Users/t/Work/Assistant/Temp/old", "trash build/out", "trash /tmp/x",
            "find /Users/t/Work/Projects/X/target -name '*.o' -exec trash {} +",
            "find /Users/t/Work/Assistant/Temp/x -exec mv {} ~/.Trash/ \\;", "find . -name '*.tmp' | xargs trash",
            "find /Users/t/Work/Projects/X -name '*.log' | xargs rm -f", "ls | xargs trash", "git ls-files -d | xargs rm",
            "find ~/Documents -name '*.pdf' -exec cp {} /Users/t/Work/Assistant/Research/ \\;", "find ~/Documents -exec mv {} /Users/t/Work/Assistant/Temp/ \\;",
            "find ~/Documents -name x | xargs grep -l foo", "find ~ -name x -exec ls -l {} +",
            "osascript -e 'tell application \"Finder\" to delete POSIX file \"/Users/t/Work/Assistant/Temp/x\"'",
            "osascript -e 'tell application \"Finder\" to open POSIX file \"/Users/t/Documents\"'",
        ] {
            assert_eq!(bash_in(c, "/Users/t/Work/Projects/Heimdall"), None, "{c}");
        }
        // The user's grant covers a plain `trash` of the exact path, never `find`, `xargs` or Finder.
        let granted = |s: &Scope| *s == Scope::Delete(PathBuf::from("/Users/t/Documents/old"));
        let r = |c: &str| refuse_with("Bash", &json!({"command": c}), &vault(), &user(), None, &granted);
        assert_eq!(r("trash /Users/t/Documents/old"), None);
        assert!(r("trash /Users/t/Documents/other").is_some());
        assert!(r("find /Users/t/Documents/old -exec trash {} +").is_some());
        assert!(r("echo /Users/t/Documents/old | xargs trash").is_some());
    }

    /// Second review, 5 Nov (HIGH): ANSI-C quotes are decoded as the shell does, so an escaped slash, a program name
    /// spelled in escapes or a NUL cannot hide a delete; a word spelled with them is never a grantable literal.
    #[test]
    fn ansi_c_escapes_are_decoded_before_judging() {
        for c in [
            "rm -rf $'\\057Users/t/Documents'", "rm -rf $'\\x2fUsers/t/Documents'", "rm -rf $'\\u002fUsers/t/Documents'",
            "rm -rf $'\\U0000002fUsers/t/Documents'", "$'\\x72m' -rf ~/x", "$'\\162\\155' -rf /Users/t/Documents", "$'\\x2fbin\\x2frm' -rf ~/Documents",
            "$'tr\\x61sh' ~/Documents", "mv ~/Documents/x $'/Users/t/.Tr\\x61sh/'", "rm -rf $'/Users/t/Docu\\x6dents\\0/../Work/Projects/X'",
            "$'\\163\\145\\143\\165\\162\\151\\164\\171' dump-keychain", "rm -rf /Users/t/$'Documents'",
        ] {
            assert!(bash(c).is_some_and(|m| m.contains("deny list")), "{c}");
        }
        // NUL ends the string as in the shell: what is left is what is deleted.
        assert_eq!(bash("rm -rf $'/Users/t/Work/Projects/X/a\\0/../../../../Documents'"), None);
        assert_eq!(bash("rm -rf $'/tmp/a\\tb' $'\\x2ftmp/x'"), None);
        let mut w = String::new();
        let chars: Vec<char> = "a\\x41\\101\\u00e9\\cA\\e\\q\\x\\'z'".chars().collect();
        let end = ansi_c(&chars, 0, &mut w);
        assert_eq!((w.as_str(), chars[end]), ("aAA\u{e9}\u{1}\u{1b}\\q\\x'z", '\''));
        // A grant names one literal path; the same path spelled in escapes is not it.
        let granted = |s: &Scope| *s == Scope::Delete(PathBuf::from("/Users/t/Documents/old"));
        let r = |c: &str| refuse_with("Bash", &json!({"command": c}), &vault(), &user(), None, &granted);
        assert_eq!(r("rm -rf /Users/t/Documents/old"), None);
        assert!(r("rm -rf $'/Users/t/Documents/old'").is_some());
        assert!(r("rm -rf $'/Users/t/Documents/old\\0/../../Desktop'").is_some(), "the escaped NUL");
        assert!(r("rm -rf $'\\x2fUsers/t/Documents/old'").is_some());
    }
}
