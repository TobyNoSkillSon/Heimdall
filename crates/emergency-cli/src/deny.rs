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
//! scopes, exactly, is let through (`refuse_with`). Keystrokes, point clicks and presses on Heimdall are refused, but for
//! the install's click on its own Quit item while a build is staged and Heimdall's GO INSTALL grant holds (`quit_click`).
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
    refuse_in(None, tool, input, vault, user, cwd, granted)
}
/// `refuse_with` for the hook of Heimdall's data folder `data` (`--data-dir`): a job may delete the memory-need files
/// in its `memory-needs/` folder without the user (bug note 2026-11-08T02-03-44, `memory_need`).
pub fn refuse_in(data: Option<&Path>, tool: &str, input: &Value, vault: &Path, user: &Path, cwd: Option<&Path>, granted: &dyn Fn(&Scope) -> bool) -> Option<String> {
    let needs = data.map(|d| lexical(&d.join(NEEDS))).into_iter().collect();
    let ctx = Ctx { vault, user, granted, needs, moved: Default::default(), keychain_used: Default::default(), other_security: Default::default(), skill_line: Default::default(), clock: Default::default(), roots_cache: Default::default(), cdpath: Default::default(), plain: Default::default(), home_set: Default::default(), cd_token: Default::default(), cd_vars: Default::default(), set_cd: Default::default(), vars: Default::default(), expanding: Default::default() };
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
folders and the build caches. If it really must go, ask the user in your final reply, naming each exact path. (Heimdall's \
relay can ask on a card instead: an option with grant.delete naming the exact paths; after their click, delete each with \
one plain rm of its absolute path from /.)";
const GUARD: &str = "Heimdall's deny list: if the variable were empty this would delete the root or a folder outside \
the Vault and scratch. Guard it as \"${NAME:?}\" or use the literal path, then run it again.";
const WEIGHTS: &str = "Heimdall's deny list: this deletes model weights, which needs the user's yes. Ask in your final reply.";
/// The folder the gate cannot know (after `cd "$X"`, `cd -`, `popd`, a `source`…): a path no delete may reach,
/// that every relative path from it stays at.
const UNKNOWN: &str = "/.heimdall-gate/unknown-folder";
/// Below `UNKNOWN`: a path that climbed out of the unknown folder with `..`, so it could be anywhere.
const ANYWHERE: &str = "..anywhere";
/// Folder names that hold protected things, in lower case: a relative path from the unknown folder through one of
/// them is treated as protected.
/// (`etc` and `System` are root-owned or sealed, so a project's own `etc/` or `System/` is left alone.)
const PROTECTED_NAMES: [&str; 3] = ["keychains", "launchagents", "launchdaemons"];
/// A line whose folder changes cannot be followed (more than 128 possible folder states).
const FOLDERS: &str = "Heimdall's deny list (docs/harness-autonomy.md): this line changes folders in too many ways to judge what it deletes. Split it into shorter commands, or name the folders in full.";
/// Judging the line took past `DEADLINE`, or it nests shells past four levels.
const SLOW: &str = "Heimdall's deny list (docs/harness-autonomy.md): this line is too long or too nested to judge in time. Split it into shorter commands.";
/// How long a line may take to judge: the hook's own timeout is 10 s, and a hook that times out lets the call run.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
/// A brace expansion too large to read.
const BRACES: &str = "Heimdall's deny list (docs/harness-autonomy.md): this brace expansion is too large to judge. Write the words out.";
const DISK: &str = "Heimdall's deny list: formatting, partitioning or writing raw disks needs the user's yes. Ask in your \
final reply.";
const KEYCHAIN: &str = "Heimdall's deny list: the Keychain is the user's. Reading or changing it needs their yes (each read \
can put a password prompt on their screen). Ask in your final reply.";
const SYSTEM: &str = "Heimdall's deny list: this changes macOS system settings, launch agents or login items, which \
needs the user's yes. Ask in your final reply. Reading the login items needs none.";
const PRESS: &str = "Heimdall's deny list: keystrokes, clicks at a point and presses on Heimdall act on the user's screen and \
their cards, and only they answer those. Drive an app through its own test hooks or accessibility on a lab build.";
const QUIT: &str = "Heimdall's deny list: Heimdall is untouchable. Only its own Quit menu item drains it safely, and only \
the integrating install task uses it, after Heimdall's GO INSTALL (docs/personal/nightly-build.md section 4). Never kill or quit \
it any other way.";
const INSTALL_ONLY: &str = "Heimdall's deny list: Heimdall is untouchable. Its Quit menu item is clicked only by the \
integrating install task, after Heimdall's GO INSTALL, with the new build staged (docs/personal/nightly-build.md section 4).";
const NO_GO: &str = "Heimdall's deny list: Heimdall is untouchable. Its Quit menu item is clicked only by the \
integrating install task within 15 minutes of Heimdall's GO INSTALL, once, with the new build staged \
(docs/personal/nightly-build.md section 4), by the session that received it. This session has no go-ahead now: if it is the \
integrator, move the stage back into the Vault and ask Heimdall; any other session leaves the install alone.";
const BTM: &str = "Heimdall's deny list: sfltool dumpbtm asks for an admin password on the user's screen. List login items \
with osascript -e 'tell application \"System Events\" to get the name of every login item', launchctl list or \
ls ~/Library/LaunchAgents instead; those need no yes.";

struct Ctx<'a> {
    vault: &'a Path,
    user: &'a Path,
    granted: &'a dyn Fn(&Scope) -> bool,
    /// Heimdall's `memory-needs/` folder, when the hook knows its data folder (`memory_need`).
    needs: Vec<PathBuf>,
    /// Where this command line moved something from outside the allowed areas (the destination, and the moved
    /// thing's new path): a later delete of it in the same line is a delete of the original (bug note 2026-11-03T18-04-11).
    moved: std::cell::RefCell<Vec<PathBuf>>,
    /// A `security` call the user's keychain grant covered, and any other `security` call in the same line.
    keychain_used: std::cell::Cell<bool>,
    other_security: std::cell::Cell<bool>,
    /// The line being judged may hold a skill copy (`skill_line`): no substitution anywhere, and every other command
    /// in it reads only or is a skill copy itself.
    skill_line: std::cell::Cell<bool>,
    /// When judging began (a line is refused past `DEADLINE`, so the hook never runs into its own timeout), and the
    /// allowed areas, read once.
    clock: std::cell::Cell<Option<std::time::Instant>>,
    roots_cache: std::cell::OnceCell<Vec<(PathBuf, usize)>>,
    /// The line sets CDPATH (`CDPATH=…`, `export CDPATH`, zsh's `cdpath=(…)`).
    cdpath: std::cell::Cell<bool>,
    /// The line has no compound command or group an `exit` could sit in without running.
    plain: std::cell::Cell<bool>,
    /// The line sets HOME.
    home_set: std::cell::Cell<bool>,
    /// The line holds `cd`, `chdir`, `pushd` or `popd` as a word anywhere (in quotes, assignments or substitutions).
    cd_token: std::cell::Cell<bool>,
    /// Variables the line assigns a folder change, and whether it gives `set --` one.
    cd_vars: std::cell::RefCell<Vec<String>>,
    /// Variables the line sets: every value each may hold where it is used (all its assignments, `+=`, `for … in`
    /// words, chains of them, `~/` expanded), or None when the line sets it in a way the gate cannot follow (`read`,
    /// `print -v`, `${NAME::=…}`, a value from a substitution).
    vars: std::cell::RefCell<Vec<(String, Option<Vec<String>>)>>,
    /// Judging the words a line variable expanded to (they are not expanded again).
    expanding: std::cell::Cell<bool>,
    set_cd: std::cell::Cell<bool>,
}

/// One simple command: its words (quotes removed) and whether each word had an expansion (`$`, backticks) outside
/// single quotes.
/// `after_pipe`: it reads the output of the command before it (`a | b`); `via_xargs`: `xargs` ran it, so more operands
/// come from its input.
#[derive(Debug, Default, Clone)]
struct Simple {
    words: Vec<String>, expands: Vec<bool>, redirects: Vec<String>, after_pipe: bool, via_xargs: bool,
    /// What ends it: `&&`, `||`, `|`, `&`, or `;`/a new line/a parenthesis.
    ends: Sep,
    /// The subshells it runs in, outermost first (`( … )`, `$( … )`, backticks, a here-document a shell reads): each
    /// one's identity, and whether it is a substitution or a body (it then leaves the line's folder and status as they
    /// were).
    levels: Vec<(u64, bool)>,
    /// `! command`: its status is the other way round.
    negated: bool,
    /// `name() …`: it defines a function. `wild`: at or after a loop or a function definition in a line that changes
    /// folder, so the folder after it is unknown.
    defines_fn: bool,
    wild: bool,
    /// `NAME=(…)`: the subshell level its parenthesis opened, whose command holds the array's words.
    array_level: Option<u64>,
    /// How many `{ … }` groups it sits in, and whether its program is its first word as written (no wrapper).
    group: usize,
    bare: bool,
    /// The program word as written, its substitutions' text included (`$(which rm)`, `$RM`): what an expanded program
    /// may be.
    hint: String,
    /// The text of the substitutions in its other words (`eval "$(direnv export zsh)"` gives `direnv export zsh`).
    hint_args: String,
    /// A brace expansion too large to read (more than 1,024 distinct words).
    unreadable: bool,
    /// The here-documents it reads on its input, and their bodies once read (`osascript <<'EOF'` scripts: bug note
    /// 2026-11-08T13-03-44, the gate never read them).
    docs: Vec<u64>,
    stdin: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Sep { #[default] Seq, And, Or, Pipe, Bg }

/// Identities for subshells, unique across nested reads of one line.
static LEVEL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn new_level() -> u64 { LEVEL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }

impl Ctx<'_> {
    fn command(&self, command: &str, cwd: Option<PathBuf>, depth: usize) -> Option<String> {
        // Past four nested shells the line is refused, not waved through (re-review 8 Nov: `eval eval eval eval eval rm …`).
        if depth > 4 { return Some(SLOW.into()); }
        if self.clock.get().is_none() { self.clock.set(Some(std::time::Instant::now())); }
        // A line inside another (`sh -c`, `eval`) may hold a copy only if the line around it may too.
        let allowed = (depth == 0 || self.skill_line.get()) && self.skill_line(command, cwd.as_deref());
        let outer = self.skill_line.replace(allowed);
        let refusal = self.command_in(command, cwd, depth);
        self.skill_line.set(outer);
        refusal
    }
    fn command_in(&self, command: &str, cwd: Option<PathBuf>, depth: usize) -> Option<String> {
        // A program word that is a substitution may expand to nothing (review 8 Nov: `` `true` rm -rf ~/Documents ``
        // and `$(true) rm …` ran the `rm` unjudged), so the words after it are judged as a command of their own too.
        // A loop or a function definition around a `cd` (re-review 8 Nov: `for … do cd ..; done`, `repeat 5 cd ..`,
        // `f() { cd ~; }; … f && rm …`): the folder after any command is unknown.
        // A line too large to read in time is refused (re-review 8 Nov: parsing ran before the deadline was checked).
        if command.len() > 128 * 1024 { return Some(SLOW.into()); }
        let mut raw = split(command);
        // From the first loop or function definition whose body changes folder (zsh's short forms included: `for i (…)
        // cd ..`, `for i in …; cd ..`, `repeat 5 cd ..`; a loop after `then`/`else`/`do` too), the folder after any
        // command is unknown; commands before it keep their folders, and a `cd` in a deeper subshell or substitution
        // than the loop ends there (re-reviews 8 Nov).
        if let Some(k) = loop_moves(&raw) { for s in &mut raw[k..] { s.wild = true; } }
        // A zsh group opened without a space whose text holds a folder change (`{cd a; echo x,y}`: zsh opens a group
        // at a command's start whatever the braces hold), or options that change how `cd` works set in the line
        // (`setopt chaselinks`, `autopushd`, `set -o …`): the folder is unknown throughout (eighth review 8 Nov).
        let options = raw.iter().any(|s| cd_options(&s.words));
        let changes = raw.iter().any(|s| s.words.iter().any(|w| matches!(w.as_str(), "cd" | "chdir" | "pushd" | "popd" | "source" | ".")));
        if attached_cd(command) || (options && changes) {
            for s in &mut raw { s.wild = true; }
        }
        // HOME or CDPATH set in the line, any way (`NAME=`, `+=`, `NAME[1]=`, `${NAME:=…}`, `${NAME::=…}`, `read NAME`,
        // `export NAME`; tenth review 8 Nov). `PYTHONHOME=` and a search word (`grep CDPATH`) do not count.
        if raw.iter().any(|s| sets_var(s, "HOME")) { self.home_set.set(true); }
        // CDPATH named anywhere except as text a reader is given (`grep CDPATH ~/.zshrc`) counts as set (eleventh
        // review 8 Nov: `set -A cdpath ~`, `x=CDPATH; typeset $x=~`, `IFS= read -r CDPATH`, `${(A)cdpath::=…}`).
        let reader = |s: &Simple| s.words.iter().find(|w| !w.contains('=')).is_some_and(|p| matches!(p.rsplit('/').next().unwrap_or(p),
            "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "cat" | "less" | "more" | "head" | "tail" | "wc" | "echo" | "file" | "stat" | "ls"));
        // Clearing it (`unset CDPATH`, `CDPATH=`, `export CDPATH=`) is the safe thing and does not count.
        let clears = |s: &Simple| s.words.iter().find(|w| !w.contains('=')).is_some_and(|p| p == "unset");
        // (Quotes are gone from words, so `CDPATH=''` reads `CDPATH=`; zsh's `cdpath=(…)` also reads `cdpath=` before
        // its parenthesis, so only the scalar with nothing after it counts as clearing.)
        let empty = |w: &str| w == "CDPATH=" && !command.contains("CDPATH=(");
        if raw.iter().any(|s| !reader(s) && !clears(s) && s.words.iter().any(|w| !empty(w) && tokens(w).any(|t| t == "CDPATH" || t == "cdpath"))) { self.cdpath.set(true); }
        // A folder-changing word anywhere in the line: a program named by an expansion (`c=cd; $c ~`, `$(echo cd) ~`,
        // `set -- cd ~; "$@"`) may be one, and `trap`, `emulate … -c`, `functions[…]=` can run one (eleventh review).
        let cd_token = tokens(command).any(|t| matches!(t, "cd" | "chdir" | "pushd" | "popd"));
        self.cd_token.set(cd_token);
        // Only when that command's own text names one (`trap 'rm -rf "$tmp"' EXIT` beside a cd is ordinary).
        let names_cd = |w: &String| tokens(w).any(|t| matches!(t, "cd" | "chdir" | "pushd" | "popd" | "source"));
        let runs_later = raw.iter().any(|s| {
            let kind = s.words.iter().any(|w| matches!(w.as_str(), "trap" | "emulate") || w.starts_with("functions[") || w.starts_with("functions=") || w.starts_with("functions+="));
            kind && s.words.iter().any(names_cd)
        });
        if runs_later { for s in &mut raw { s.wild = true; } }
        // Variables assigned a folder change in the line (`c=cd`, `CMD='cd ~'`), and `set --` given one: a program
        // named through them may be it.
        let cd_vars: Vec<String> = raw.iter().flat_map(|s| s.words.iter()).filter_map(|w| w.split_once('=').filter(|(n, v)| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && tokens(v).any(|t| matches!(t, "cd" | "chdir" | "pushd" | "popd" | "source"))).map(|(n, _)| n.to_owned())).collect();
        let set_cd = raw.iter().any(|s| s.words.first().is_some_and(|w| w == "set") && s.words.iter().any(|w| matches!(w.as_str(), "cd" | "chdir" | "pushd" | "popd")));
        *self.cd_vars.borrow_mut() = cd_vars;
        self.collect_vars(&raw);
        self.set_cd.set(set_cd);
        // `exit` ends a path only on a plain line: no if/case/loop, function or `{ … }` group it could sit in unrun
        // (re-review 8 Nov: `[ -d x ] || { echo; exit 1; }; rm …`, `if …; then exit 1; fi; rm …`, `f() { exit; }`).
        let compound = raw.iter().any(|s| s.defines_fn || s.words.first().is_some_and(|w| matches!(w.as_str(),
            "if" | "then" | "else" | "elif" | "fi" | "case" | "esac" | "for" | "while" | "until" | "do" | "done" | "select" | "repeat" | "foreach" | "function")))
            || braces_group(command);
        self.plain.set(!compound);
        let simples = raw.into_iter().flat_map(|s| {
            // `!` after `then`, `else`, `do` or `time` negates too (seventh review).
            let negated = s.words.iter().take_while(|w| matches!(w.as_str(), "then" | "else" | "elif" | "do" | "if" | "while" | "until" | "time" | "!")).any(|w| w == "!");
            let first = s.words.first().cloned();
            let mut cur = strip_wrappers(s);
            cur.negated = negated;
            cur.bare = cur.words.first() == first.as_ref();
            let mut all = vec![cur.clone()];
            // At most three leading expansions are looked behind (re-review 8 Nov: thousands of `$x` words grew memory
            // with their square).
            while cur.expands.first() == Some(&true) && cur.words.len() > 1 && all.len() < 4 {
                cur.words.remove(0);
                cur.expands.remove(0);
                cur = strip_wrappers(cur);
                all.push(cur.clone());
            }
            all
        }).collect::<Vec<_>>();
        self.simples(&simples, cwd, depth, command)
    }
    /// The simple commands of one line, in order, from `cwd`. Every folder the shell may be in is followed, with how
    /// the last command ended: a `cd` may fail, so after one both folders are possible, and an `&&`/`||` runs or skips
    /// what follows by that status (a pipeline as one). A subshell's `cd` ends with it; a substitution leaves the line as
    /// it was. Re-reviews 8 Nov: `cd /tmp/never-made; rm -rf *`, `(cd x); rm …`, `cd a || cd b && rm …`, and a line of
    /// many `cd`s judged in time linear in its folders (past 128 folder states the line is refused).
    fn simples(&self, simples: &[Simple], cwd: Option<PathBuf>, depth: usize, command: &str) -> Option<String> {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Status { Ok, Fail, Unknown }
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Ran { Yes, No, Maybe }
        // A state: the folder, how the last command ended, whether it ran, and the folders `pushd` saved in this line.
        type State = (Option<PathBuf>, Status, Ran, Vec<Option<PathBuf>>);
        let mut states: Vec<State> = vec![(cwd, Status::Ok, Ran::Yes, Vec::new())];
        // Each open subshell: its identity, the states and the last command before it, and whether it is a
        // substitution. A subshell's first command follows the separator before it (re-review 8 Nov: `cd X && ( rm … )`).
        let mut stack: Vec<(u64, Vec<State>, Option<&Simple>, bool)> = Vec::new();
        let mut prev: Option<&Simple> = None;
        for simple in simples {
            if simple.unreadable { return Some(BRACES.into()); }
            let shared = stack.iter().zip(&simple.levels).take_while(|(open, (id, _))| open.0 == *id).count();
            while stack.len() > shared {
                let Some((_, outer, outer_prev, subst)) = stack.pop() else { break };
                // A substitution leaves the line as it was; a subshell restores the folder and its status is unknown.
                states = if subst { outer } else { outer.into_iter().map(|(c, _, _, d)| (c, Status::Unknown, Ran::Maybe, d)).collect() };
                prev = if subst { outer_prev } else { None };
            }
            for &(id, subst) in &simple.levels[stack.len()..] { stack.push((id, states.clone(), prev, subst)); }
            let after = prev.map_or(Sep::Seq, |p| p.ends);
            let mut next: Vec<State> = Vec::new();
            let mut add = |st: State| if !next.contains(&st) { next.push(st); };
            for (cwd, status, ran, dirs) in &states {
                let runs = if simple.after_pipe { *ran } else {
                    match (after, status) {
                        (Sep::And, Status::Fail) | (Sep::Or, Status::Ok) => Ran::No,
                        (Sep::And | Sep::Or, Status::Unknown) => Ran::Maybe,
                        _ => Ran::Yes,
                    }
                };
                if runs == Ran::No { add((cwd.clone(), *status, Ran::No, dirs.clone())); continue; }
                if let Some(r) = self.one(simple, prev, cwd.as_deref(), depth, command) { return Some(r); }
                if runs == Ran::Maybe { add((cwd.clone(), *status, Ran::No, dirs.clone())); }
                // `exit` or `return` that runs ends this path (`cd X || exit 1` then `rm …`).
                // Behind a wrapper (`command exit`, `env exit`, `nice exit`) it is no exit of this shell (seventh review).
                let exits = simple.bare && simple.words.first().is_some_and(|w| matches!(w.as_str(), "exit" | "logout"));
                if exits && self.plain.get() && simple.levels.is_empty() && !simple.after_pipe && !matches!(simple.ends, Sep::Pipe | Sep::Bg) && runs == Ran::Yes { continue; }
                if simple.wild { add((Some(PathBuf::from(UNKNOWN)), Status::Unknown, runs, dirs.clone())); }
                let (ok, fail) = if simple.negated { (Status::Fail, Status::Ok) } else { (Status::Ok, Status::Fail) };
                // `pushd` saves the folder and `popd` returns to it (sixth review: `pushd crates && … && popd; rm …`).
                let program = simple.words.first().map(|w| w.rsplit('/').next().unwrap_or(w)).unwrap_or("");
                let in_shell = !simple.after_pipe && !matches!(simple.ends, Sep::Bg | Sep::Pipe);
                if program == "popd" && in_shell && simple.words.len() == 1 {
                    match dirs.split_last() {
                        Some((back, rest)) => add((back.clone(), ok, runs, rest.to_vec())),
                        // Nothing saved in this line: each command runs in a fresh shell, so its stack is empty and `popd`
                        // fails where it is (seventh review: `pushd x; make; popd; rm -rf build`).
                        None => add((cwd.clone(), fail, runs, Vec::new())),
                    }
                    continue;
                }
                match self.moves_folder(simple, cwd.as_deref()) {
                    // In the background, or before a pipe, a `cd` runs in a subshell; as a pipeline's last command zsh
                    // runs it in the shell itself.
                    Some(_) if simple.ends == Sep::Bg || simple.ends == Sep::Pipe => add((cwd.clone(), Status::Unknown, runs, dirs.clone())),
                    Some(to) if simple.after_pipe => { add((to, Status::Unknown, runs, dirs.clone())); add((cwd.clone(), Status::Unknown, runs, dirs.clone())); }
                    Some(to) => {
                        let mut saved = dirs.clone();
                        if program == "pushd" { saved.push(cwd.clone()); }
                        add((to, ok, runs, saved));
                        add((cwd.clone(), fail, runs, dirs.clone()));
                    }
                    None => add((cwd.clone(), Status::Unknown, runs, dirs.clone())),
                }
            }
            // After `;`, a new line or `&`, the next command runs whatever this one did: only the folders matter.
            if matches!(simple.ends, Sep::Seq | Sep::Bg) {
                let mut folders: Vec<State> = Vec::new();
                for (c, _, _, d) in next { let st = (c, Status::Unknown, Ran::Yes, d); if !folders.contains(&st) { folders.push(st); } }
                next = folders;
            }
            // Relative `cd`s that may each fail double the folders (eight make 256): past 128 the line is refused at once.
            if next.len() > 128 { return Some(FOLDERS.into()); }
            states = next;
            prev = Some(simple);
        }
        None
    }
    /// Where a command moves the shell's folder: `cd`, `chdir`, `pushd` (their flags skipped), `popd`, or an `eval`
    /// of one. `Some(None)` is a folder from an expansion (not known, as before); `/` stands for one the gate cannot
    /// know (`cd -`, `popd`, `pushd +1`, an `eval`): `UNKNOWN`, so a relative delete after it is refused.
    fn moves_folder(&self, simple: &Simple, cwd: Option<&Path>) -> Option<Option<PathBuf>> {
        let program = simple.words.first()?;
        let unknown = Some(PathBuf::from(UNKNOWN));
        // A program named by an expansion that may be a folder change: its text names one (`$(echo cd)`), it names a
        // variable the line assigned one (`c=cd; $c ~`), or it is `"$@"` after `set -- cd …` (eleventh review).
        // `$PYTHON`, `${CC:-cc}`, `"$EDITOR"` beside a cd are left alone.
        if simple.expands.first() == Some(&true) {
            let hint = &simple.hint;
            let named = tokens(hint).any(|t| matches!(t, "cd" | "chdir" | "pushd" | "popd" | "source") || self.cd_vars.borrow().iter().any(|v| v == t));
            let args = self.set_cd.get() && (hint.contains("$@") || hint.contains("$*") || hint.contains("${@") || hint.contains("${*"));
            if named || args { return Some(unknown); }
        }
        match program.rsplit('/').next().unwrap_or(program) {
            "cd" | "chdir" | "pushd" => {
                let mut i = 1;
                while simple.words.get(i).is_some_and(|w| w.len() > 1 && w.starts_with('-') && w != "--") { i += 1; }
                if simple.words.get(i).is_some_and(|w| w == "--") { i += 1; }
                // `cd old new` replaces text in the folder's name; `~-` is the last folder, `~name` another user's;
                // CDPATH (set in the line or before it) can send a relative `cd` anywhere (re-review 8 Nov).
                let cdpath = std::env::var_os("CDPATH").is_some_and(|v| !v.is_empty());
                // `-P` resolves links (seventh review: `cd -P docs/..` through a link into ~/Documents).
                let physical = simple.words[1..i].iter().any(|w| !w.starts_with("--") && w.contains('P'));
                let home = self.home_set.get();
                Some(match simple.words.get(i) {
                    None if program == "pushd" || home => unknown,
                    None => Some(self.user.to_path_buf()),
                    Some(a) if home && (a.starts_with('~') || a.contains("HOME")) => unknown,
                    Some(_) if simple.words.len() > i + 1 => unknown,
                    Some(a) if a == "-" || a.starts_with('+') || (a.starts_with('~') && a != "~" && !a.starts_with("~/")) => unknown,
                    // An expansion: `$HOME/…` is known; anything else is a folder the gate cannot know, so relative
                    // deletes after it are refused (seventh review: `cd "$HOME/Documents" && rm -rf *`, `cd $D && …`).
                    Some(a) if simple.expands.get(i).copied().unwrap_or(false) && self.expand_vars(a).is_some_and(|w| !w.contains('$') && !w.contains('`') && !w.contains('…')) => {
                        let w = self.expand_vars(a).unwrap_or_default();
                        self.absolute(&w, cwd).or(unknown)
                    }
                    Some(a) if simple.expands.get(i).copied().unwrap_or(false) => {
                        let rest = a.strip_prefix("$HOME/").or_else(|| a.strip_prefix("${HOME}/")).or(if a == "$HOME" || a == "${HOME}" { Some("") } else { None });
                        match rest { Some(r) if !r.contains('$') && !r.contains('`') && !r.contains('…') => Some(normalize(&self.user.join(r))), _ => unknown }
                    }
                    Some(a) if !a.starts_with('/') && !a.starts_with('~') && (cdpath || self.cdpath.get()) => unknown,
                    Some(a) if physical => {
                        let raw = if a.starts_with('/') { Some(PathBuf::from(a)) } else if let Some(r) = a.strip_prefix("~/") { Some(self.user.join(r)) } else if a == "~" { Some(self.user.to_path_buf()) } else { cwd.map(|c| c.join(a)) };
                        raw.and_then(|r| std::fs::canonicalize(r).ok()).or(unknown)
                    }
                    Some(a) => self.absolute(a, cwd),
                })
            }
            "popd" | "repeat" => Some(unknown),
            // A sourced file runs in this shell and may change folder or how `cd` works: the folder after it is unknown.
            // Reading the file to judge it proved unsafe (tenth review 8 Nov: the line can rewrite it first, through
            // `=( … )` or a here-document, and a pipe can hang the reader), so it is never read.
            "." | "source" => Some(unknown),
            // zsh's short `if [[ … ]] cd x` and `while [[ … ]] cd ..` (seventh review).
            "[[" | "[" | "test" if simple.words.iter().skip(1).any(|w| matches!(w.as_str(), "cd" | "chdir" | "pushd" | "popd")) => Some(unknown),
            // An `eval` of anything expanded (`eval "$CMD"`, `eval "$(cat go.sh)"`) may change folder; so may one whose
            // text names a change (eleventh review).
            "eval" if simple.words.iter().any(|w| tokens(w).any(|t| matches!(t, "cd" | "chdir" | "pushd" | "popd" | "source"))) => Some(unknown),
            // An `eval` of something expanded may change folder, except the environment tools' output agents eval
            // (`eval "$(direnv export zsh)"`, `ssh-agent -s`, `brew shellenv`, `pyenv init -`…).
            "eval" if simple.expands.iter().skip(1).any(|e| *e) && !env_tool(&simple.hint_args) => Some(unknown),
            _ => None,
        }
    }
    /// One simple command judged from `cwd`, the command before it `prev`.
    fn one(&self, simple: &Simple, prev: Option<&Simple>, cwd: Option<&Path>, depth: usize, command: &str) -> Option<String> {
        if self.clock.get().is_some_and(|t| t.elapsed() > DEADLINE) { return Some(SLOW.into()); }
        // Redirects first: a line of only a redirect (`> ~/Library/LaunchAgents/x.plist`, `&>file`) writes too
        // (re-review 8 Nov).
        for target in &simple.redirects {
            if target.starts_with("/dev/disk") || target.starts_with("/dev/rdisk") { return Some(DISK.into()); }
            for p in self.paths(target, cwd) {
                if let Some(r) = self.keychain(&p).or_else(|| self.system_path(&p)) { return Some(r); }
            }
        }
        let Some(program) = simple.words.first() else { return None };
        let name = program.rsplit('/').next().unwrap_or(program).to_owned();
        let args = &simple.words[1..];
        // A program named by an expansion (`$(which rm)`, `$RM`, `$SHELL`): unknown until it runs (re-reviews 8 Nov).
        // Its `-c` script (`-c`, `-lc`, `-ec`) is judged as a shell's, and when what it is written as names a remover
        // (`rm`, `rmdir`, `unlink`, `srm`, `trash`: `$RM`, `$(which rm)`, `${RM_CMD}`) its paths are delete targets, as
        // `rm`'s, whatever its flags. Other programs by expansion (`$GREP -r`, `$PIP install -r`) are left alone.
        if simple.expands.first() == Some(&true) {
            if let Some(i) = args.iter().position(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))) {
                if let Some(r) = args.get(i + 1).and_then(|s| self.command(s, cwd.map(Path::to_path_buf), depth + 1)) { return Some(r); }
            }
            let remover = simple.hint.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).any(|t| DELETERS.contains(&t) || t == "rmdir");
            if remover {
                let targets: Vec<(String, bool)> = args.iter().filter(|a| !a.starts_with('-')).map(|a| (a.clone(), false)).collect();
                if let Some(r) = self.judge_deletes("rm", &simple.words, &targets, cwd) { return Some(r); }
            }
        }
        if matches!(name.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh") {
            if let Some(i) = args.iter().position(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))) {
                if let Some(r) = args.get(i + 1).and_then(|s| self.command(s, cwd.map(Path::to_path_buf), depth + 1)) { return Some(r); }
            }
            return None;
        }
        if name == "eval" {
            if let Some(r) = self.command(&args.join(" "), cwd.map(Path::to_path_buf), depth + 1) { return Some(r); }
            return None;
        }
        if name == "osascript" && { let all = osascript_script(args, &simple.stdin).to_lowercase(); all.contains("login item") || all.contains("loginitem") } {
            let script = osascript_script(args, &simple.stdin);
            match login_item_removal(&script) {
                Some(item) if (self.granted)(&Scope::LoginItem(item.clone())) => return None,
                Some(_) => return Some(SYSTEM.into()),
                None if login_item_writes(&script) => return Some(SYSTEM.into()),
                None => {}
            }
        }
        if name == "sfltool" && args.first().is_some_and(|a| a == "dumpbtm") { return Some(BTM.into()); }
        // --- Quit for install (bug notes 2026-11-08T13-03-44, 12-27-04; the user: "Allow it") ---
        // Quitting Heimdall any other way (a quit event, killall, pkill) skips its drain and is refused; the exact click
        // on its own Quit menu item passes only while an install has its new build staged and a GO INSTALL grant (`quit_click`, `installing`).
        if name == "osascript" && quits(&osascript_script(args, &simple.stdin)) { return Some(QUIT.into()); }
        if matches!(name.as_str(), "killall" | "pkill") && kills_heimdall(args) { return Some(QUIT.into()); }
        if name == "osascript" {
            let script = osascript_script(args, &simple.stdin);
            if presses(&script) {
                if !quit_click(&script) { return Some(PRESS.into()); }
                if !self.installing() { return Some(INSTALL_ONLY.into()); }
                // Review 1110 item 3: and only with Heimdall's own go-ahead, a grant it wrote when it sent GO INSTALL
                // (`grants::write_quit`, 15 minutes, once). A stage alone is something any agent can make.
                if !(self.granted)(&Scope::QuitInstall) { return Some(NO_GO.into()); }
            }
        }
        // --- end quit for install ---
        if name == "cliclick" { return Some(PRESS.into()); }
        if name == "mv" {
            if let Some(r) = self.moves_to_scratch(args, cwd, command.contains("mktemp")) { return Some(r); }
            // `… | xargs mv {} ~/.Trash/`: what it moves comes from its input; into the Trash or a temporary folder
            // that is a delete of each.
            if simple.via_xargs && self.moves_into_scratch(args, cwd) {
                let fed: Vec<(String, bool)> = fed(&simple, prev).into_iter().map(|w| (w, false)).collect();
                if let Some(r) = self.judge_deletes("mv", &simple.words, &fed, cwd) { return Some(r); }
            }
        }
        if name == "osascript" { if let Some(r) = self.finder_deletes(&osascript_script(args, &simple.stdin), cwd) { return Some(r); } }
        if name == "security" {
            if self.keychain_grant(&simple) { self.keychain_used.set(true); return None; }
            self.other_security.set(true);
        }
        if let Some(r) = self.disk(&name, args).or_else(|| self.security(&name, args)).or_else(|| self.system(&name, args)) {
            return Some(r);
        }
        if let Some(r) = self.deletes(&name, &simple, cwd, prev) { return Some(r); }
        // Any other program handed a Keychain file.
        // (Path-like words only, and the deadline checked: resolving each one on disk costs time.)
        for a in args.iter().filter(|a| a.contains('/') || a.contains("keychain") || a.contains('$')) {
            if self.clock.get().is_some_and(|t| t.elapsed() > DEADLINE) { return Some(SLOW.into()); }
            for p in self.paths(a, cwd) {
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
                for p in self.paths(a, cwd) {
                    // A copy *from* a protected place is a read; only the last argument (or tee's/sed's files) is written.
                    let written = matches!(name.as_str(), "tee" | "touch" | "mkdir" | "chmod" | "chown" | "plutil" | "sed") || Some(a) == args.last();
                    if written { if let Some(r) = self.system_path(&p) { return Some(r); } }
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
            // update-skills' guarded copy of one skill folder into a T3 agent's skills or Heimdall's own (the user clicked
            // "Allow skill copy", then "Allow it" for Heimdall's folder, 8 Nov; approvals.log refused it six times):
            // its delete stays inside that skill's folder.
            "rsync" if !simple.via_xargs && self.skill_copy(simple, cwd) => return None,
            // Every operand after the first may be the target (options may follow the paths, and a substitution after
            // the real target can expand to nothing: review 8 Nov), an option's value is none (`--exclude .git`,
            // `-e ssh`: re-review 8 Nov), and `--remove-source-files` deletes the sources too.
            "rsync" if words.iter().any(|w| w.starts_with("--del") || w == "--remove-source-files") => {
                let operands = rsync_operands(words);
                let skip = if words.iter().any(|w| w == "--remove-source-files") { 0 } else { 1 };
                operands.into_iter().skip(skip).map(own).collect()
            }
            _ => return None,
        };
        // `… | xargs rm`: the paths come from the command before it (`fed`).
        if simple.via_xargs { targets.extend(fed(simple, prev).into_iter().map(|w| (w, false))); }
        self.judge_deletes(name, words, &targets, cwd)
    }

    /// `rsync [-a…] --delete <Vault>/Projects/Heimdall/skills/<workers|shared>/<name>/ ~/.claude/skills/<name>/` (or
    /// `~/.agents/skills/<name>/`), and `<…>/skills/<heimdall|shared>/<name>/ ~/Library/Application
    /// Support/Heimdall/agent/skills/<name>/`: the update-skills skill's copy of one skill into the folders Claude Code,
    /// Codex, OpenCode and Grok load, or into Heimdall's own (the user's "Allow it", 8 Nov ~05:10). Only that: the
    /// repository's set that place loads as the source, the same skill name in that place as the target, both with
    /// their trailing slash (so `--delete` reaches only inside
    /// `<name>/`), literal paths with no `..`, no expansion, no remote host, known flags only, and no symlink on the way
    /// to either folder or inside them (update-skills: "A link belongs to someone else: preserve it"; a skill
    /// changed locally is the copying agent's to check, as the skill says). Anything else stays a delete outside the
    /// Vault.
    fn skill_copy(&self, simple: &Simple, cwd: Option<&Path>) -> bool {
        self.skill_line.get() && self.skill_copy_alone(simple, cwd)
    }
    /// Whether `command` may hold a skill copy (review 8 Nov: the symlink check runs before the line does, so an
    /// earlier command in it could plant a link, `mkdir … && ln -s ~/Documents ~/.agents/skills/documents && rsync …`,
    /// or a substitution could, `` `ln -s …` ``). No substitution or process substitution anywhere, and every command
    /// is one that only reads (`cd`, `diff`, `ls`, `test`, `[`, `pwd`, `true`, `find` without an action) with no
    /// redirect, or a skill copy itself. The `cwd` each copy sees is followed through the `cd`s.
    fn skill_line(&self, command: &str, cwd: Option<&Path>) -> bool {
        // No substitution, process substitution or `$` at all: a parameter expansion can assign (`${CDPATH:=/x}`).
        if command.contains('`') || command.contains('$') || command.contains("<(") || command.contains(">(") { return false; }
        // No group, subshell, function, pipe or background job (re-review 8 Nov: `(cd x); …`, `cd x &`, `cd () {…}`,
        // `cd x | true` each leave zsh's folder where it was while a followed `cd` would move the gate's).
        if command.chars().any(|c| matches!(c, '(' | ')' | '{' | '}' | '|')) || command.replace("&&", "").contains('&') { return false; }
        let mut cwd = cwd.map(Path::to_path_buf);
        // The raw words, never through `strip_wrappers` (review 8 Nov): an assignment (`HOME=/x`, `CDPATH=/x`,
        // `PATH=/x`) or a wrapper (`env`, `command`, `exec`, `nice`…) is its own program word here, and none is allowed.
        for simple in split(command) {
            let Some(program) = simple.words.first() else { if simple.redirects.is_empty() { continue } else { return false } };
            if !simple.redirects.is_empty() || simple.via_xargs { return false; }
            // Bare names only: `./ls` or `/tmp/x/ls` is some other program.
            let ok = match program.as_str() {
                "cd" => {
                    // A `cd` may fail, or be skipped after `||` or `[ … ] &&` (re-review 8 Nov: a failed `cd` into a
                    // skill folder not made yet, then `rsync … ./`, would empty the session's folder). So the folder
                    // after one is unknown, and a copy after it names its folders in full.
                    if simple.words.len() > 2 || simple.words.get(1).is_some_and(|a| a.starts_with('-')) { return false; }
                    cwd = None;
                    true
                }
                "diff" | "ls" | "test" | "[" | "pwd" | "true" => true,
                "find" => !simple.words.iter().any(|w| matches!(w.as_str(), "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint" | "-fprint0" | "-fprintf" | "-fls")),
                "rsync" => self.skill_copy_alone(&simple, cwd.as_deref()),
                _ => false,
            };
            if !ok { return false; }
        }
        true
    }
    /// One `rsync` that is the skill copy, judged alone (`skill_copy` also needs the line to allow it). `--delete` is
    /// optional (approvals.log line 109 chained a `--delete` copy with plain ones); a plain copy outside this shape
    /// could plant a link a later copy follows, so it is not allowed beside one.
    fn skill_copy_alone(&self, simple: &Simple, cwd: Option<&Path>) -> bool {
        let words = &simple.words;
        if words.first().map(String::as_str) != Some("rsync") || simple.expands.iter().any(|e| *e) || simple.after_pipe { return false; }
        let flags_ok = words[1..].iter().filter(|w| w.starts_with('-')).all(|w| {
            matches!(w.as_str(), "--archive" | "--recursive" | "--times" | "--verbose" | "--quiet" | "--checksum" | "--itemize-changes"
                | "--dry-run" | "--human-readable" | "--progress" | "--stats" | "--delete" | "--delete-after" | "--delete-during"
                | "--delete-delay" | "--delete-before")
                || (w.len() > 1 && !w.starts_with("--") && w[1..].chars().all(|c| "avrtqcinhP".contains(c)))
        });
        if !flags_ok { return false; }
        let ops: Vec<&String> = words[1..].iter().filter(|w| !w.starts_with('-')).collect();
        let [src, dst] = ops[..] else { return false };
        let plain = |w: &str| w.ends_with('/') && !w.contains(':') && !Path::new(w).components().any(|c| c == Component::ParentDir);
        if !plain(src) || !plain(dst) { return false; }
        // The destination is named in full, never from the folder the gate believes the shell is in.
        if !(dst.starts_with('/') || dst.starts_with("~/")) { return false; }
        let (Some(src), Some(dst)) = (self.absolute(src, cwd), self.absolute(dst, cwd)) else { return false };
        let Ok(rest) = src.strip_prefix(self.vault.join("Projects/Heimdall/skills")) else { return false };
        let parts: Vec<&str> = rest.iter().filter_map(|p| p.to_str()).collect();
        let [set, name] = parts[..] else { return false };
        let named = !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !name.starts_with('-');
        if !named { return false; }
        // Each place loads its own sets (update-skills' table): the T3 agents `workers/` and `shared/`, Heimdall's Pi
        // `heimdall/` and `shared/` (the personal edition; `public/` is the public edition's and stays out).
        let places: [(&str, &[&str]); 3] = [
            (".claude/skills", &["workers", "shared"]),
            (".agents/skills", &["workers", "shared"]),
            ("Library/Application Support/Heimdall/agent/skills", &["heimdall", "shared"]),
        ];
        let into = places.iter().any(|(d, sets)| sets.contains(&set) && dst == self.user.join(d).join(name));
        // The source is a real skill now: a folder (not a link) holding a SKILL.md file, so an empty folder made for
        // the purpose cannot empty another tool's skill of that name.
        let real = std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir())
            && std::fs::symlink_metadata(src.join("SKILL.md")).is_ok_and(|m| m.is_file());
        into && real && no_link_below(self.user, &dst) && no_link_below(self.vault, &src) && no_link_inside(&dst) && no_link_inside(&src)
    }

    /// Each delete target judged by `deletable`; a refused one passes only on the user's grant for exactly that path.
    fn judge_deletes(&self, name: &str, words: &[String], targets: &[(String, bool)], cwd: Option<&Path>) -> Option<String> {
        for (raw, may_grant) in targets {
            if self.clock.get().is_some_and(|t| t.elapsed() > DEADLINE) { return Some(SLOW.into()); }
            // A variable the line assigned a literal value is put in (`D=~/Documents; rm -rf "$D/x"`); such a target is
            // never grantable.
            // A variable the line sets: each value it may hold is judged once (no expanding again: `D='$D'` would never
            // end). One the gate cannot follow (`ROOT=$(pwd)`, `read f`, `${f%.png}`) falls to the rule for unknowable
            // words below, as a variable from outside the line does (correctness review 8 Nov).
            if raw.contains('$') && !self.expanding.get() && self.uses_line_var(raw) {
                if let Some(words) = self.expand_all(raw) {
                    let each: Vec<(String, bool)> = words.into_iter().map(|w| (w, false)).collect();
                    self.expanding.set(true);
                    let r = self.judge_deletes(name, words_of(&each).as_slice(), &each, cwd);
                    self.expanding.set(false);
                    if r.is_some() { return r; }
                    continue;
                }
            }
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
            // A delete *inside* a folder (`link/`, `link/*`, `*` after `cd link`) reaches through a link to it: the folder
            // is judged where it really is too (re-review 8 Nov: a link in the Vault to ~/Documents).
            let into = raw.ends_with('/') || prefix != *raw || raw.is_empty();
            let through = into && std::fs::canonicalize(&path).is_ok_and(|real| real != path && !self.deletable(&real));
            let refusal = if path.starts_with(self.user.join(".cache/huggingface")) { Some(WEIGHTS) }
                else if !self.deletable(&path) || through { Some(if by_variable { GUARD } else { DELETE }) }
                else { None };
            let Some(refusal) = refusal else { continue };
            // A job releasing the memory it declared (bug note 2026-11-08T02-03-44): one need file, as written.
            if refusal == DELETE && *may_grant && prefix == *raw && self.memory_need(name, words, raw, &path) { continue; }
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
        // A source through a line variable is each value it may hold (sixteenth review: `D=~/Documents; mv "$D" /tmp/`).
        let expanded: Vec<String> = sources.iter().flat_map(|s| {
            if s.contains('$') && self.uses_line_var(s) { self.expand_all(s).unwrap_or_else(|| vec![UNKNOWN.to_owned()]) } else { vec![(*s).clone()] }
        }).collect();
        for s in &expanded {
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

    /// A memory-need file a job wrote and now releases (bug note 2026-11-08T02-03-44: the gate declined the Vella
    /// task's and the relay's `rm` of `memory-needs/angel-nemotron-wer.json`, and Jarvis stayed out until the user's click):
    /// a plain `rm` or `unlink` (no `-r`) of one absolute literal `<data>/memory-needs/<name>.json`, directly in that
    /// folder, with no symlink on the way, that is a regular file or already gone. Never the folder, a glob, another
    /// file in it, or the data folder's other files.
    fn memory_need(&self, name: &str, words: &[String], raw: &str, path: &Path) -> bool {
        let recursive = words.iter().skip(1).any(|w| w == "--recursive" || w == "--dir" || (w.starts_with('-') && !w.starts_with("--") && w.contains(['r', 'R', 'd'])));
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        matches!(name, "rm" | "unlink") && !recursive && !self.needs.is_empty()
            && raw.starts_with('/') && exact(raw) && !raw.ends_with('/')
            && !raw[1..].split('/').any(|c| c.is_empty() || c == "." || c == "..")
            && file.len() > ".json".len() && file.ends_with(".json") && !file.starts_with('.')
            && path.parent().is_some_and(|folder| self.needs.iter().any(|n| n == folder))
            // The folder as it really is: a `memory-needs` that is a link elsewhere is no need folder.
            && path.parent().and_then(|f| std::fs::symlink_metadata(f).ok()).is_none_or(|m| m.is_dir())
            && self.needs.iter().any(|n| resolved(path).parent() == Some(std::fs::canonicalize(n).unwrap_or_else(|_| n.clone()).as_path()))
            && std::fs::symlink_metadata(path).map_or(true, |m| m.file_type().is_file())
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
        self.roots_cache.get_or_init(|| self.roots_now()).clone()
    }
    fn roots_now(&self) -> Vec<(PathBuf, usize)> {
        let mut roots = vec![(self.vault.to_path_buf(), 2)];
        for t in ["/tmp", "/private/tmp", "/var/folders", "/private/var/folders"] { roots.push((PathBuf::from(t), 1)); }
        if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) { roots.push((lexical(Path::new(&t)), 1)); }
        for c in ["Library/Developer/Xcode/DerivedData", "Library/Caches", ".cache", ".t3/worktrees", ".t3/scratch"] { roots.push((self.user.join(c), 1)); }
        let resolved: Vec<(PathBuf, usize)> = roots.iter().filter_map(|(r, d)| std::fs::canonicalize(r).ok().map(|r| (r, *d))).collect();
        roots.extend(resolved);
        roots
    }

    // The disk ignores letter case (`~/library/launchagents` is the real folder; fourteenth review 8 Nov), so these
    // checks compare paths in lower case.
    fn keychain(&self, path: &Path) -> Option<String> {
        // Through a link too (fifteenth review: a project link to ~/Library/LaunchAgents, /Volumes/Macintosh HD).
        if !path.starts_with(UNKNOWN) { let real = resolved(path); if real != path { if let Some(r) = self.keychain_at(&real) { return Some(r); } } }
        self.keychain_at(path)
    }
    fn keychain_at(&self, path: &Path) -> Option<String> {
        let low = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        if let Ok(tail) = path.strip_prefix(UNKNOWN) {
            let tail = low(tail);
            let any = tail.iter().any(|c| c == "keychains") || tail.extension().is_some_and(|e| e == "keychain-db" || e == "keychain");
            return any.then(|| KEYCHAIN.into());
        }
        let p = low(&lexical(path));
        (p.starts_with(low(&self.user.join("Library/Keychains"))) || p.starts_with("/library/keychains")).then(|| KEYCHAIN.into())
    }

    fn system_path(&self, path: &Path) -> Option<String> {
        if !path.starts_with(UNKNOWN) { let real = resolved(path); if real != path { if let Some(r) = self.system_path_at(&real) { return Some(r); } } }
        self.system_path_at(path)
    }
    fn system_path_at(&self, path: &Path) -> Option<String> {
        let low = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        if let Ok(tail) = path.strip_prefix(UNKNOWN) {
            // From the unknown folder: through a launch-agent or launch-daemon folder, or an Apple preference file.
            let tail = low(tail);
            let name = tail.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let any = tail.iter().any(|c| PROTECTED_NAMES.iter().any(|n| c == *n))
                || (tail.iter().any(|c| c == "preferences") && (name.starts_with("com.apple.") || name.starts_with(".globalpreferences")));
            return any.then(|| SYSTEM.into());
        }
        let p = low(&lexical(path));
        let user_prefs = p.strip_prefix(low(&self.user.join("Library/Preferences"))).ok().and_then(|r| r.to_str().map(str::to_owned)).is_some_and(|r| r.starts_with("com.apple.") || r.starts_with(".globalpreferences"));
        let protected = p.starts_with(low(&self.user.join("Library/LaunchAgents")))
            || ["/library/launchagents", "/library/launchdaemons", "/etc", "/private/etc", "/system", "/library/preferences"].iter().any(|r| p.starts_with(r))
            || user_prefs;
        protected.then(|| SYSTEM.into())
    }

    /// The variables the line sets, in order (fifteenth review 8 Nov: `D=…; D+=/../..; rm -rf "$D"` was judged by the
    /// first value; `OUT=$ROOT/out`, `for D in ~/Documents`, `read D` were not followed). Each assignment adds to what
    /// a variable may hold, as the line's branches are not followed.
    fn collect_vars(&self, raw: &[Simple]) {
        let name_ok = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !n.starts_with(|c: char| c.is_ascii_digit());
        let tilde = |v: &str| -> String {
            if let Some(r) = v.strip_prefix("~/") { self.user.join(r).display().to_string() } else if v == "~" { self.user.display().to_string() } else { v.to_owned() }
        };
        let set = |name: &str, vals: Option<Vec<String>>, add: bool| {
            let mut vars = self.vars.borrow_mut();
            match vars.iter_mut().find(|(n, _)| n == name) {
                Some((_, old)) => {
                    *old = match (old.take(), vals) {
                        (Some(mut o), Some(v)) => { if add { for x in v { if !o.contains(&x) { o.push(x); } } Some(o) } else { Some(v) } }
                        _ => None,
                    };
                    if old.as_ref().is_some_and(|o| o.len() > 64) { *old = None; }
                }
                None => vars.push((name.to_owned(), vals.filter(|v| v.len() <= 64))),
            }
        };
        for (at, s) in raw.iter().enumerate() {
            // Past the keywords a command can follow (`then D=build`, `else D=~/x`, `do …`; sixteenth review).
            let skip = s.words.iter().take_while(|w| matches!(w.as_str(), "then" | "else" | "elif" | "do" | "if" | "while" | "until" | "!" | "{")).count();
            let words = &s.words[skip..].to_vec();
            let expands = &s.expands[skip..].to_vec();
            // `for NAME in w…`: each word (a substitution's or glob's output is a name in the folder: `*`).
            if words.first().is_some_and(|w| w == "for") && words.get(2).is_some_and(|w| w == "in") && words.get(1).is_some_and(|n| name_ok(n)) {
                let vals: Option<Vec<String>> = words[3..].iter().zip(&expands[3..]).map(|(w, e)| {
                    if w.contains("$(…)") || w.contains("`…`") { Some(vec!["*".to_owned()]) }
                    else if *e { self.expand_all(w) } else { Some(vec![tilde(w)]) }
                }).collect::<Option<Vec<Vec<String>>>>().map(|v| v.concat());
                set(&words[1], vals, true);
            }
            let program_at = words.iter().position(|w| !w.split_once('=').is_some_and(|(n, _)| name_ok(n.trim_end_matches('+')))).unwrap_or(words.len());
            let declares = words.get(program_at).is_some_and(|p| matches!(p.as_str(), "export" | "typeset" | "local" | "declare" | "readonly" | "integer" | "float"));
            for (k, (w, e)) in words.iter().zip(expands.iter()).enumerate() {
                // `NAME=v` and `NAME+=v` before the program, or as a declaring builtin's arguments.
                if k < program_at || (declares && k > program_at) {
                    if let Some((n, v)) = w.split_once('=') {
                        let (n, append) = match n.strip_suffix('+') { Some(n) => (n, true), None => (n, false) };
                        if !name_ok(n) { continue; }
                        // An array, `NAME=(a b)`: its words are the next command one level down (sixteenth review).
                        let array = v.is_empty() && k + 1 == words.len() && s.array_level.is_some();
                        // `$(mktemp …)` with no folder of its own names a temporary folder (`tmp=$(mktemp -d)`).
                        // This word's own substitution text: the first word's is in `hint`, each later word's a line of
                        // `hint_args` (correctness review: `local a=$(mktemp -d) b=$(pwd)` took b for a temporary folder).
                        let subst_text = if k == 0 && skip == 0 { s.hint.split_once("$(…)").or_else(|| s.hint.split_once("`…`")).map_or(String::new(), |(_, t)| t.to_owned()) } else {
                            let before = words[..k].iter().skip(usize::from(skip == 0)).filter(|x| x.contains("$(…)") || x.contains("`…`")).count();
                            s.hint_args.lines().nth(before).unwrap_or("").to_owned()
                        };
                        // `mktemp …` (with `2>/dev/null || mktemp …` fallbacks) whose templates sit in a temporary folder.
                        let temp_arg = |a: &str| !a.contains('/') || ["/tmp/", "/private/tmp/", "$TMPDIR", "${TMPDIR"].iter().any(|p| a.trim_matches('"').starts_with(p));
                        let mktemp = (v == "$(…)" || v == "`…`") && subst_text.split("||").all(|alt| {
                            let alt = alt.replace("2>/dev/null", "");
                            let mut it = alt.split_whitespace();
                            it.next() == Some("mktemp") && it.all(temp_arg)
                        });
                        let vals = if array {
                            // Its words: the command at exactly its own level (none for `NAME=()`); a substitution in it
                            // makes the array unknown.
                            let items = raw[at + 1..].iter().find(|n| n.levels.len() == s.levels.len() + 1 && n.levels.last().map(|l| l.0) == s.array_level);
                            match items {
                                None => Some(vec![]),
                                Some(items) if items.words.iter().any(|w| w.contains("$(…)") || w.contains("`…`")) => None,
                                Some(items) => items.words.iter().zip(&items.expands).map(|(w, e)| if *e { self.expand_all(w) } else { Some(vec![tilde(w)]) }).collect::<Option<Vec<Vec<String>>>>().map(|v| v.concat()),
                            }
                        } else if mktemp { Some(vec!["/tmp/heimdall-gate-mktemp".to_owned()]) }
                        else if w.contains("$(…)") || w.contains("`…`") { None } else if *e { self.expand_all(v) } else { Some(vec![tilde(v)]) };
                        let vals = if append && array {
                            // `NAME+=(…)` adds elements.
                            let old = self.vars.borrow().iter().find(|(m, _)| m == n).map(|(_, o)| o.clone()).unwrap_or(Some(vec![]));
                            match (old, vals) { (Some(mut o), Some(v)) => { o.extend(v); Some(o) } _ => None }
                        } else if append {
                            let old = self.vars.borrow().iter().find(|(m, _)| m == n).map(|(_, o)| o.clone()).unwrap_or(Some(vec![String::new()]));
                            match (old, vals) { (Some(o), Some(v)) => Some(o.iter().flat_map(|a| v.iter().map(move |b| format!("{a}{b}"))).collect()), _ => None }
                                .filter(|v: &Vec<String>| v.iter().all(|x| x.len() <= 4096))
                        } else { vals };
                        set(n, vals, true);
                    }
                }
                // `${NAME:=…}`, `${NAME::=…}`, `${NAME=…}` assign as they expand.
                for (i, _) in w.match_indices("${") {
                    let rest = &w[i + 2..];
                    let n: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if name_ok(&n) && (rest[n.len()..].starts_with(":=") || rest[n.len()..].starts_with("::=") || rest[n.len()..].starts_with('=')) { set(&n, None, true); }
                }
            }
            // Builtins that set a variable the gate cannot follow.
            if let Some(p) = words.get(program_at) {
                let named: Vec<&String> = match p.as_str() {
                    "read" | "vared" | "getopts" => words[program_at + 1..].iter().filter(|w| !w.starts_with('-')).collect(),
                    "print" | "printf" => words.iter().position(|w| w == "-v").and_then(|i| words.get(i + 1)).into_iter().collect(),
                    "set" if words.iter().any(|w| w == "-A" || w == "+A") => words[program_at + 1..].iter().filter(|w| !w.starts_with('-') && !w.starts_with('+')).take(1).collect(),
                    _ => vec![],
                };
                for n in named { if name_ok(n) { set(n, None, true); } }
            }
        }
    }
    /// Every word `word` may be with the line's variables (and `$HOME`) put in; None when one is not known (set in a
    /// way the gate cannot follow, or not set in the line), or past 64 words.
    fn expand_all(&self, word: &str) -> Option<Vec<String>> {
        let chars: Vec<char> = word.chars().collect();
        let mut out = vec![String::new()];
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '$' { for o in &mut out { o.push(chars[i]); } i += 1; continue; }
            let braced = chars.get(i + 1) == Some(&'{');
            let start = i + 1 + usize::from(braced);
            let end = (start..chars.len()).find(|&j| !(chars[j].is_ascii_alphanumeric() || chars[j] == '_')).unwrap_or(chars.len());
            if end == start { return None; }
            let name: String = chars[start..end].iter().collect();
            let values = if name == "HOME" { vec![self.user.display().to_string()] }
                else { self.vars.borrow().iter().find(|(n, _)| *n == name).and_then(|(_, v)| v.clone())? };
            // A subscript (`$dirs[1]`, `${dirs[@]}`) stands for any element; `${NAME:?}`/`${NAME?}` is the value or
            // an error. Other modifiers are not followed.
            let mut next = end;
            if chars.get(next) == Some(&'[') { next = (next..chars.len()).find(|&j| chars[j] == ']').map_or(chars.len(), |j| j + 1); }
            if braced {
                let close = (next..chars.len()).find(|&j| chars[j] == '}')?;
                let modifier: String = chars[next..close].iter().collect();
                next = close + 1;
                // `${NAME:-word}`/`${NAME-word}`: the value or the word (both are judged).
                if let Some(d) = modifier.strip_prefix(":-").or_else(|| modifier.strip_prefix('-')) {
                    if d.contains('$') || d.contains('`') { return None; }
                    let mut both = values.clone();
                    both.push(d.to_owned());
                    out = out.iter().flat_map(|o| both.iter().map(move |v| format!("{o}{v}"))).collect();
                    if out.len() > 64 || out.iter().any(|o| o.len() > 4096) { return None; }
                    i = next;
                    continue;
                }
                if !(modifier.is_empty() || modifier.starts_with(":?") || modifier.starts_with('?')) { return None; }
            }
            out = out.iter().flat_map(|o| values.iter().map(move |v| format!("{o}{v}"))).collect();
            // At most 64 words of at most 4,096 characters (sixteenth review: doubling a value grew without bound).
            if out.len() > 64 || out.iter().any(|o| o.len() > 4096) { return None; }
            i = next;
        }
        Some(out)
    }
    /// `word` with the line's variables put in, when that gives exactly one word.
    fn expand_vars(&self, word: &str) -> Option<String> {
        self.expand_all(word).filter(|v| v.len() == 1).and_then(|mut v| v.pop())
    }
    /// Whether `word` uses a variable the line sets, and only such variables (or `$HOME`): one from outside the line
    /// (`$S`, `$TMPDIR`) leaves the word to the rules for unknowable words.
    fn uses_line_var(&self, word: &str) -> bool {
        let vars = self.vars.borrow();
        let names: Vec<String> = word.match_indices('$').map(|(i, _)| {
            let rest = word[i + 1..].trim_start_matches('{');
            rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
        }).collect();
        names.iter().any(|n| vars.iter().any(|(m, _)| m == n)) && names.iter().all(|n| n == "HOME" || vars.iter().any(|(m, _)| m == n))
    }
    /// Every path `word` may name: its variables put in, each made absolute.
    fn paths(&self, word: &str, cwd: Option<&Path>) -> Vec<PathBuf> {
        if word.contains('$') && self.uses_line_var(word) {
            return self.expand_all(word).unwrap_or_default().iter().filter_map(|w| self.absolute(w, cwd)).collect();
        }
        self.absolute(word, cwd).into_iter().collect()
    }
    /// A word as an absolute path (`~` and `$HOME` expanded, relative ones from `cwd`); None when it cannot be known.
    fn absolute(&self, word: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        // From a folder the gate cannot know, a relative path stays unknown, `..` and `tmp/…` included (eleventh
        // review 8 Nov: the unknown folder was `/`, so `cd "$X" && rm -rf tmp/build` was judged as /tmp/build).
        // The relative path is kept below the sentinel so protected names in it are still seen (thirteenth review:
        // `cd "$H" && cp x Library/LaunchAgents/`); one that climbs out with `..` could be anywhere.
        let relative = !(word.starts_with('/') || word.starts_with('~') || word.starts_with("$HOME") || word.starts_with("${HOME}"));
        if relative && cwd.is_some_and(|c| c.starts_with(UNKNOWN)) {
            if word.is_empty() || word.contains('$') { return Some(PathBuf::from(UNKNOWN)); }
            if Path::new(word).components().any(|c| c == Component::ParentDir) {
                let rest: PathBuf = Path::new(word).components().filter(|c| matches!(c, Component::Normal(_))).collect();
                return Some(Path::new(UNKNOWN).join(ANYWHERE).join(rest));
            }
            let tail = cwd.and_then(|c| c.strip_prefix(UNKNOWN).ok()).map(Path::to_path_buf).unwrap_or_default();
            return Some(normalize(&Path::new(UNKNOWN).join(tail).join(word)));
        }
        let home_rel = word.strip_prefix("~/").or_else(|| word.strip_prefix("$HOME/")).or_else(|| word.strip_prefix("${HOME}/"));
        let path = if let Some(rest) = home_rel { self.user.join(rest) }
            else if matches!(word, "~" | "$HOME" | "${HOME}") { self.user.to_path_buf() }
            else if word.starts_with('/') { PathBuf::from(word) }
            else if word.contains('$') {
                // A variable the line assigned one literal value (`LA=~/Library/LaunchAgents; cp a "$LA/x"`).
                return self.expand_vars(word).and_then(|w| (!w.contains('$')).then(|| self.absolute(&w, cwd)).flatten());
            }
            else if word.is_empty() || word.starts_with('~') { return None }
            else { cwd?.join(word) };
        Some(normalize(&path))
    }
}

/// No folder from just below `base` down to `path` (itself included) is a symlink. A part that does not exist yet is
/// none.
fn no_link_below(base: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(base) else { return false };
    let mut p = base.to_path_buf();
    for part in rest.iter() {
        p.push(part);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) { return false; }
    }
    true
}
/// Nothing inside `dir` is a symlink (a folder that does not exist yet holds none). A tree past 20,000 entries is not
/// looked through and counts as having one.
fn no_link_inside(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            seen += 1;
            if seen > 20_000 { return false; }
            match e.file_type() {
                Ok(t) if t.is_symlink() => return false,
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
    true
}

/// The folder in Heimdall's data folder where jobs declare memory (`speak::memory_line::NEEDS_DIR`).
pub const NEEDS: &str = "memory-needs";

/// The operands of an `rsync` (word indexes): its words that are neither options nor an option's separate value.
fn rsync_operands(words: &[String]) -> Vec<usize> {
    const VALUED: [&str; 44] = ["--exclude", "--include", "--filter", "--exclude-from", "--include-from", "--files-from",
        "--log-file", "--log-file-format", "--backup-dir", "--compare-dest", "--copy-dest", "--link-dest", "--partial-dir",
        "--temp-dir", "--suffix", "--rsh", "--rsync-path", "--chmod", "--chown", "--timeout", "--contimeout", "--bwlimit",
        "--max-size", "--min-size", "--max-delete", "--modify-window", "--out-format", "--password-file", "--port",
        "--sockopts", "--usermap", "--groupmap", "--iconv", "--info", "--debug", "--skip-compress", "--compress-level",
        "--block-size", "--checksum-choice", "--protocol", "--outbuf", "--address", "--stop-at", "--remote-option"];
    let mut out = Vec::new();
    let mut i = 1;
    let mut options = true;
    while i < words.len() {
        let w = &words[i];
        if options && w == "--" { options = false; }
        else if options && w.starts_with("--") { if VALUED.contains(&w.as_str()) { i += 1; } }
        // A short cluster: its first letter that takes a value (`e`, `f`, `T`, `B`, `M`) takes the rest of the word, or
        // the next word when it is last (`-e ssh`, `-ae ssh`, `-f'- .DS_Store'` whole: re-review 8 Nov).
        else if options && w.starts_with('-') && w.len() > 1 {
            let letters: Vec<char> = w[1..].chars().collect();
            if letters.iter().position(|c| "efTBM".contains(*c)).is_some_and(|p| p + 1 == letters.len()) { i += 1; }
        }
        else { out.push(i); }
        i += 1;
    }
    out
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
/// The script an `osascript` runs: its `-e` lines, else the here-document it reads (`stdin`), else its words.
fn osascript_script(args: &[String], stdin: &str) -> String {
    let lines: Vec<&str> = args.windows(2).filter(|w| w[0] == "-e").map(|w| w[1].as_str()).collect();
    if !lines.is_empty() { lines.join("\n") } else if !stdin.trim().is_empty() { stdin.to_owned() } else { args.join(" ") }
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

// --- Quit for install (bug notes 2026-11-08T13-03-44 and 2026-11-08T12-27-04) ---
impl Ctx<'_> {
    /// An install is under way: its new build is staged beside the installed one (docs/personal/nightly-build.md section 4
    /// stages it before quitting; a hand rollback stages the previous build the same way).
    fn installing(&self) -> bool {
        self.user.join("Applications/.Heimdall.app.stage/Contents/Info.plist").is_file()
    }
}
/// Whether a script quits Heimdall outside its menu: a `quit` (outside strings) in a script that names it
/// (`quit app "Heimdall"`, `tell application "Heimdall" to quit`, JXA's `Application('Heimdall').quit()`). The app has
/// no handler for the quit event, so it would stop without draining.
fn quits(script: &str) -> bool {
    let lower = script.to_lowercase();
    (lower.contains("\"heimdall") || lower.contains("'heimdall")) && script_words(script).iter().any(|w| w == "quit")
}
/// `killall Heimdall`, `pkill -x Heimdall`, `pkill -f …/Heimdall.app/…`: a signal to the running app.
fn kills_heimdall(args: &[String]) -> bool {
    args.iter().filter(|a| !a.starts_with('-')).any(|a| { let l = a.to_lowercase(); l == "heimdall" || l.contains("heimdall.app") })
}
/// Whether the script is exactly a click on Heimdall's own Quit menu item ("Quit Heimdall", "Quit <name>" after a
/// rename), told to its process through System Events, optionally after the click that opens its menu and short
/// delays, and nothing else. ~13:05: "Allow it", for an approved install only.
fn quit_click(script: &str) -> bool {
    let lines: Vec<String> = script.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()).filter(|l| !l.is_empty()).collect();
    const SYS: &str = "tell application \"system events\"";
    const PROC: &str = "tell process \"heimdall\"";
    let one_line = format!("{SYS} to {PROC} to ");
    let opened = format!("{SYS} to {PROC}");
    let body: Vec<&str> = match lines.as_slice() {
        [only] => match only.strip_prefix(one_line.as_str()) { Some(s) => vec![s], None => return false },
        [first, rest @ .., last] if *first == opened && last == "end tell" => rest.iter().map(String::as_str).collect(),
        [first, second, rest @ .., a, b] if first == SYS && second == PROC && a == "end tell" && b == "end tell" => rest.iter().map(String::as_str).collect(),
        _ => return false,
    };
    let mut steps = body.iter().filter(|l| !is_delay(l));
    let (Some(last), opens) = (steps.next_back(), steps.collect::<Vec<_>>()) else { return false };
    let opener = |l: &str| l.strip_prefix("click ").and_then(menu_bar_item).is_some_and(str::is_empty);
    opens.len() <= 1 && opens.iter().all(|l| opener(l)) && quit_item(last)
}
/// `delay 0.4`: a short wait, at most five seconds.
fn is_delay(line: &str) -> bool {
    line.strip_prefix("delay ").and_then(|n| n.parse::<f64>().ok()).is_some_and(|n| (0.0..=5.0).contains(&n))
}
/// `menu bar item N of menu bar M` at the start of `s` (N and M small numbers); what follows it.
fn menu_bar_item(s: &str) -> Option<&str> {
    let small = |n: &str| !n.is_empty() && n.len() <= 2 && n.bytes().all(|b| b.is_ascii_digit());
    let rest = s.strip_prefix("menu bar item ")?;
    let (n, rest) = rest.split_once(" of menu bar ")?;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    (small(n) && small(&rest[..end])).then(|| &rest[end..])
}
/// The click on the Quit item: `click menu item "Quit X" of menu 1 of <menu bar item>`, or
/// `click (first menu item of menu 1 of <menu bar item> whose name begins with "Quit ")`.
fn quit_item(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("click menu item \"quit ") {
        let Some((name, rest)) = rest.split_once('"') else { return false };
        return !name.trim().is_empty() && !name.contains('\\') && rest.strip_prefix(" of menu 1 of ").and_then(menu_bar_item).is_some_and(str::is_empty);
    }
    let Some(rest) = line.strip_prefix("click (first menu item of menu 1 of ").and_then(menu_bar_item) else { return false };
    matches!(rest, " whose name begins with \"quit \")" | " whose name starts with \"quit \")")
}
// --- end quit for install ---

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
                    "env" | "nohup" | "time" | "command" | "exec" | "builtin" | "noglob" | "nocorrect" | "!" => 1 + s.words[1..].iter().take_while(|w| w.starts_with('-') || (first == "env" && w.contains('='))).count(),
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

/// The shell's brace expansion of one word: `a{b,c}d` is `abd acd`, nested and repeated braces included; a brace
/// without a comma at its own level stays as it is (`{}`, `${x}`). Repeated words are kept once (they judge the same),
/// and past 1,024 distinct words or 20,000 steps the expansion is too large to read (None; re-review 8 Nov: dropping
/// words past a cut hid a target, and `{,}` repeated grew without bound).
fn brace_expand(word: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut budget = 20_000usize;
    expand_into(word, &mut out, &mut budget).then_some(out)
}
fn expand_into(word: &str, out: &mut Vec<String>, budget: &mut usize) -> bool {
    if *budget == 0 { return false; }
    *budget -= 1;
    let chars: Vec<char> = word.chars().collect();
    for open in 0..chars.len() {
        if chars[open] != '{' || (open > 0 && chars[open - 1] == '$') { continue; }
        let (mut depth, mut commas) = (0usize, Vec::new());
        for (j, &c) in chars.iter().enumerate().skip(open) {
            match c {
                '{' => depth += 1,
                ',' if depth == 1 => commas.push(j),
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        if commas.is_empty() { break; }
                        let (pre, post): (String, String) = (chars[..open].iter().collect(), chars[j + 1..].iter().collect());
                        let mut cuts = vec![open];
                        cuts.extend(&commas);
                        cuts.push(j);
                        let mut seen: Vec<String> = Vec::new();
                        for w in cuts.windows(2) {
                            let next = format!("{pre}{}{post}", chars[w[0] + 1..w[1]].iter().collect::<String>());
                            if !seen.contains(&next) { seen.push(next); }
                        }
                        return seen.iter().all(|w| expand_into(w, out, budget));
                    }
                }
                _ => {}
            }
        }
    }
    if !out.iter().any(|w| w == word) { out.push(word.to_owned()); }
    out.len() <= 1024
}

/// A here-document's tag as the shell reads it: quoted parts whole, spaces included (re-review 8 Nov: `<<'EO F'` was
/// read as `EO`); `quoted` when any part was quoted or escaped (the body is then not expanded); `dash` for `<<-`.
struct Heredoc { tag: String, quoted: bool, dash: bool }
impl Heredoc {
    /// Read from just after `<<`; returns the tag and where reading stopped.
    fn read(chars: &[char], mut i: usize) -> (Heredoc, usize) {
        let dash = chars.get(i) == Some(&'-');
        if dash { i += 1; }
        while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') { i += 1; }
        let (mut tag, mut quoted, mut q) = (String::new(), false, None::<char>);
        while let Some(&c) = chars.get(i) {
            match (q, c) {
                (Some(o), c) if c == o => q = None,
                (Some(_), c) => tag.push(c),
                (None, '\'' | '"') => { q = Some(c); quoted = true; }
                (None, '\\') => { quoted = true; if let Some(&n) = chars.get(i + 1) { tag.push(n); i += 1; } }
                (None, c) if c.is_whitespace() || matches!(c, ';' | '|' | '&' | ')' | '<' | '>') => break,
                (None, c) => tag.push(c),
            }
            i += 1;
        }
        (Heredoc { tag, quoted, dash }, i)
    }
    /// Whether `line` ends the body: the tag alone on it, after leading tabs for `<<-` only (re-review 8 Nov: an
    /// indented `  EOF` inside the body is not the end).
    fn ends(&self, line: &str) -> bool {
        (if self.dash { line.trim_start_matches('\t') } else { line }) == self.tag
    }
}

/// A shell that runs what it reads on its input: its program a shell, with no script file or `-c` among its words
/// (wrappers such as `env` or `nice` in front are looked through).
/// `-s` reads its input even with arguments after it (`bash -s -- x`), and a shell named by an expansion that says it
/// is one (`$SHELL`, `${ZSH}`) counts (re-review 8 Nov).
fn reads_stdin_shell(words: &[String]) -> bool {
    let s = strip_wrappers(Simple { words: words.to_vec(), expands: vec![false; words.len()], ..Default::default() });
    let Some(program) = s.words.first() else { return false };
    let name = program.rsplit('/').next().unwrap_or(program);
    let lower = name.to_lowercase();
    let shell = matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh") || (name.starts_with('$') && (lower.contains("shell") || lower.ends_with("sh") || lower.ends_with("sh}")));
    let options: Vec<&String> = s.words[1..].iter().take_while(|w| w.starts_with('-') && *w != "--").collect();
    let script = |w: &&String| *w == "-c" || (!w.starts_with("--") && w.ends_with('c'));
    let stdin = options.iter().any(|w| *w == "-s" || (!w.starts_with("--") && w.contains('s')));
    shell && !options.iter().any(script) && (stdin || s.words.len() == 1 + options.len())
}

/// Whether `{` at `at` opens a zsh group written without a space (`{cd a}`): something follows it, and the braces up
/// to their match hold no `,` or `..` at their own level (that would be a brace expansion).
fn attached_group(chars: &[char], at: usize, matches: &[Option<usize>]) -> bool {
    if chars.get(at + 1).is_none_or(|n| n.is_whitespace() || *n == '}') { return false; }
    let Some(end) = matches[at] else { return false };
    // A `,` or `..` at the braces' own level makes a brace expansion; inner braces are skipped through their match.
    let mut j = at + 1;
    while j < end {
        match chars[j] {
            '{' => { j = matches[j].unwrap_or(end); }
            ',' => return false,
            '.' if chars.get(j + 1) == Some(&'.') => return false,
            _ => {}
        }
        j += 1;
    }
    true
}

/// Each `{`'s matching `}` (quotes not considered), found in one pass (tenth review 8 Nov: scanning from every `{`
/// made a line of unmatched braces quadratic).
fn brace_matches(chars: &[char]) -> Vec<Option<usize>> {
    let mut out = vec![None; chars.len()];
    let mut open = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        match c { '{' => open.push(i), '}' => if let Some(o) = open.pop() { out[o] = Some(i); }, _ => {} }
    }
    out
}

/// The first loop or function definition whose body changes folder: where the folder stops being known. A body is a
/// `do … done`, a `{ … }` group, or (zsh's short forms) the next command at the loop's level; `repeat N cd …` holds
/// it in itself. A `cd` deeper in a subshell or substitution than the loop does not count.
fn loop_moves(raw: &[Simple]) -> Option<usize> {
    let moves = |s: &Simple| s.words.iter().any(|w| matches!(w.as_str(), "cd" | "chdir" | "pushd" | "popd"));
    let lead = |s: &Simple| s.words.iter().map(String::as_str).find(|w| !matches!(*w, "then" | "else" | "elif" | "do" | "if" | "!" | "time")).map(str::to_owned);
    let body = |k: usize| -> std::ops::Range<usize> {
        let level = raw[k].levels.len();
        let Some(j) = (k + 1..raw.len()).find(|&j| raw[j].levels.len() <= level) else { return k + 1..k + 1 };
        if raw[j].words.first().is_some_and(|w| w == "do") {
            let mut nest = 0usize;
            for e in j..raw.len() {
                if raw[e].words.iter().any(|w| w == "do") { nest += 1; }
                if raw[e].words.first().is_some_and(|w| w == "done") { nest = nest.saturating_sub(1); if nest == 0 { return k..e + 1; } }
            }
            return k..raw.len();
        }
        if raw[j].group > raw[k].group {
            let end = (j..raw.len()).find(|&e| raw[e].group <= raw[k].group).unwrap_or(raw.len());
            return k..end;
        }
        k..j + 1
    };
    // A loop's body: from the loop to its `done` when a `do` follows (wherever its condition ends: `for (( … ))`,
    // `while a; b; do`), else (zsh's short forms) the rest of the line (eighth review 8 Nov).
    let loop_body = |k: usize| -> std::ops::Range<usize> {
        let Some(j) = (k + 1..raw.len()).find(|&j| raw[j].words.first().is_some_and(|w| w == "do")) else { return k..raw.len() };
        let mut nest = 0usize;
        for e in j..raw.len() {
            if raw[e].words.first().is_some_and(|w| w == "do") { nest += 1; }
            if raw[e].words.first().is_some_and(|w| w == "done") { nest = nest.saturating_sub(1); if nest == 0 { return k..e + 1; } }
        }
        k..raw.len()
    };
    for k in 0..raw.len() {
        let kw = lead(&raw[k]);
        let level = raw[k].levels.len();
        let inside = |r: std::ops::Range<usize>| raw[r].iter().any(|s| moves(s) && s.levels.len() <= level);
        match kw.as_deref() {
            Some("repeat") if moves(&raw[k]) => return Some(k),
            Some("for" | "while" | "until" | "select" | "foreach" | "repeat") if inside(loop_body(k)) => return Some(k),
            Some("function") if inside(body(k)) => return Some(k),
            _ if raw[k].defines_fn && inside(body(k)) => return Some(k),
            _ => {}
        }
    }
    None
}

/// Whether a line opens a zsh group without a space (`{` at a command's start followed by text) whose text up to
/// its matching `}` holds `cd`, `chdir`, `pushd` or `popd` as a word.
fn attached_cd(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    // Braces inside quotes are text (ninth review: `jq '.x | {cd: .y}'`).
    let mut quoted = vec![false; chars.len()];
    let mut q: Option<char> = None;
    let mut k = 0;
    while k < chars.len() {
        match (q, chars[k]) {
            (Some(o), c) if c == o => q = None,
            (None, '\'' | '"') => q = Some(chars[k]),
            (None, '\\') => { k += 1; }
            _ => {}
        }
        if k < chars.len() { quoted[k] = q.is_some(); }
        k += 1;
    }
    let ends = brace_matches(&chars);
    (0..chars.len()).any(|i| {
        if quoted[i] || chars[i] != '{' || chars.get(i + 1).is_none_or(|n| n.is_whitespace() || *n == '}') { return false; }
        if !(i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], ';' | '&' | '|' | '(' | ')' | '{')) { return false; }
        // An unmatched `{` holds the rest of the line; a group's text past 4,096 characters counts as holding one.
        let end = ends[i].unwrap_or(chars.len());
        if end - i > 4096 { return true; }
        let text: String = chars[i + 1..end].iter().collect();
        text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| matches!(w, "cd" | "chdir" | "pushd" | "popd" | "source"))
            || text.starts_with(". ") || text.contains("; . ") || text.contains("&& . ")
    })
}

/// Whether every substitution in an `eval`'s words runs an environment tool whose output only sets variables and
/// functions that do not change folder: direnv, ssh-agent, gpg-agent, brew shellenv, pyenv/rbenv/nodenv/goenv/jenv
/// init, conda shell hooks, mise/rtx/fnm/opam env.
fn env_tool(substitutions: &str) -> bool {
    const TOOLS: [&str; 14] = ["direnv", "ssh-agent", "gpg-agent", "brew", "pyenv", "rbenv", "nodenv", "goenv", "jenv", "conda", "mise", "rtx", "fnm", "opam"];
    let lines: Vec<&str> = substitutions.lines().filter(|l| !l.trim().is_empty()).collect();
    // One plain command each: nothing chained after the tool (thirteenth review: `$(brew shellenv; echo cd ~)`);
    // its errors may go to /dev/null.
    let lines: Vec<String> = lines.iter().map(|l| l.replace("2>/dev/null", "").replace(">/dev/null", "").replace("2>&1", "")).collect();
    !lines.is_empty() && lines.iter().all(|l| {
        let program = l.split_whitespace().next().unwrap_or("");
        TOOLS.contains(&program.rsplit('/').next().unwrap_or(program))
            && !l.contains([';', '&', '|', '`', '$', '>', '<']) && !tokens(l).any(|t| matches!(t, "cd" | "chdir" | "pushd" | "popd" | "source"))
    }) && !substitutions.lines().any(|l| l.contains('\n'))
}

/// The words of a list of delete targets, for `judge_deletes`' grant rules (never grantable here).
fn words_of(targets: &[(String, bool)]) -> Vec<String> { targets.iter().map(|(w, _)| w.clone()).collect() }

/// The words of a text by letters, digits and `_` (`a=cd;x` gives `a`, `cd`, `x`).
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|t| !t.is_empty())
}

/// Whether a command sets the shell variable `name`: an assignment word (`NAME=`, `NAME+=`, `NAME[…]=`), a
/// parameter expansion that assigns (`${NAME:=…}`, `${NAME::=…}`, `${NAME=…}`), or a builtin given it by name
/// (`read`, `export`, `typeset`, `declare`, `local`, `readonly`, `vared`, `print -v`, `printf -v`, `getopts`, `unset`).
fn sets_var(s: &Simple, name: &str) -> bool {
    let assigns = s.words.iter().any(|w| {
        [format!("{name}="), format!("{name}+="), format!("{name}[")].iter().any(|p| w.starts_with(p.as_str()))
            || [format!("${{{name}:="), format!("${{{name}::="), format!("${{{name}=")].iter().any(|p| w.contains(p.as_str()))
    });
    // The program past leading assignments (`IFS= read -r NAME`), and zsh's `set -A NAME` (eleventh review).
    let at = s.words.iter().position(|w| !w.contains('=') || w.starts_with('-')).unwrap_or(s.words.len());
    let builtin = s.words.get(at).is_some_and(|p| matches!(p.as_str(), "read" | "export" | "typeset" | "declare" | "local" | "readonly" | "vared" | "print" | "printf" | "getopts" | "unset" | "set"))
        && s.words.iter().skip(at + 1).any(|w| w == name || w.starts_with(&format!("{name}=")));
    assigns || builtin
}

/// Whether a command changes an option that changes how `cd` works, wherever it sits in the command (after `then`,
/// `builtin`, `noglob`…): `setopt`/`unsetopt` of one, `set -o`/`+o` one, zsh's one-letter forms (`-w` chaselinks, `-N`
/// autopushd, `-J` autocd, `-T` cdablevars), `options[…]=`, or `emulate` (ninth review 8 Nov). `set -o pipefail` and
/// `set -e` change nothing of the sort.
fn cd_options(words: &[String]) -> bool {
    const CD: [&str; 9] = ["chaselinks", "chasedots", "autopushd", "pushdminus", "pushdtohome", "cdablevars", "autocd", "posixcd", "pushdignoredups"];
    let norm = |w: &str| w.to_lowercase().replace('_', "");
    let named = |w: &str| { let n = norm(w); let n = n.strip_prefix("no").unwrap_or(&n).to_owned(); CD.contains(&n.as_str()) };
    // Inside an `eval`'s words too (`eval 'setopt chaselinks'`; eleventh review).
    if words.first().is_some_and(|w| w == "eval") {
        let inner: Vec<String> = words[1..].iter().flat_map(|w| w.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).collect();
        if cd_options(&inner) { return true; }
    }
    words.iter().enumerate().any(|(i, w)| match w.as_str() {
        // An option named through an expansion may be any of them; `setopt -w` is the one-letter form.
        "setopt" | "unsetopt" => words[i + 1..].iter().any(|a| named(a) || a.contains('$') || a.contains('`')
            || (a.starts_with('-') || a.starts_with('+')) && a.len() > 1 && a[1..].chars().any(|c| matches!(c, 'w' | 'N' | 'J' | 'T'))),
        "emulate" => true,
        "set" => {
            let rest = &words[i + 1..];
            rest.iter().enumerate().any(|(j, a)| ((a == "-o" || a == "+o") && rest.get(j + 1).is_some_and(|o| named(o) || o.contains('$') || o.contains('`')))
                || ((a.starts_with('-') || a.starts_with('+')) && !a.starts_with("--") && a.len() > 1 && a[1..].chars().any(|c| matches!(c, 'w' | 'N' | 'J' | 'T'))))
        }
        w => w.starts_with("options[") || w.starts_with("options=") || w.starts_with("options+="),
    })
}

/// Whether a line holds a `{ … }` group: a `{` standing alone as a word (quotes not considered, so this only errs
/// toward finding one).
fn braces_group(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    chars.iter().enumerate().any(|(i, &c)| c == '{'
        && (i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], ';' | '&' | '|' | '(' | ')'))
        && chars.get(i + 1).is_none_or(|n| n.is_whitespace()))
}

/// The commands an unquoted here-document's body runs: its `$(…)` and backtick substitutions (the rest is text).
fn body_substitutions(body: &str) -> Vec<Simple> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = subst_end(&chars, i + 1);
                out.extend(split(&chars[(i + 2).min(end)..end].iter().collect::<String>()));
                i = end;
            }
            '`' => {
                let mut end = i + 1;
                while end < chars.len() && chars[end] != '`' { if chars[end] == '\\' { end += 1; } end += 1; }
                out.extend(split(&chars[i + 1..end.min(chars.len())].iter().collect::<String>()));
                i = end;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// A shell command line cut into simple commands at `;`, `&`, `|`, new lines, parentheses, braces and `$(` outside
/// quotes. Here-document bodies are skipped unless a shell reads them. Redirect targets are kept apart.
#[allow(unused_assignments)] // the macros' last resets
fn split(command: &str) -> Vec<Simple> {
    // Substitutions nested past 64 deep are not read (re-review 8 Nov: 4,000 nested `$(` took 7 s to parse).
    thread_local! { static NESTING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    let nesting = NESTING.with(|n| { n.set(n.get() + 1); n.get() });
    struct Leave;
    impl Drop for Leave { fn drop(&mut self) { NESTING.with(|n| n.set(n.get().saturating_sub(1))); } }
    let _leave = Leave;
    if nesting > 64 { return vec![Simple { unreadable: true, ..Default::default() }]; }
    let mut out = Vec::new();
    let mut cur = Simple::default();
    let mut word = String::new();
    let mut started = false;
    let mut expands = false;
    let mut redirect = false;
    // The word holds an unquoted `{`: the shell's brace expansion applies (`{rm,-rf}`, `-delete{,}`: re-review 8 Nov).
    let mut brace = false;
    // Subshells open around the current command, inside `(( … ))` arithmetic (where `<<` is a shift), and the text of
    // the current word's substitutions.
    let mut levels: Vec<(u64, bool)> = Vec::new();
    let mut arith = false;
    // `{ … }` groups open, and those opened zsh's way without a space (`{cd a}`), closed by a `}` ending a word.
    let mut group = 0usize;
    let mut attached = 0usize;
    let mut subst_raw = String::new();
    let mut heredocs: Vec<(Heredoc, bool, u64)> = Vec::new();
    // Where this line's commands begin in `out`: a here-document feeds a shell that reads it through a pipe too.
    let mut line_start = 0usize;
    let chars: Vec<char> = command.chars().collect();
    let matches = brace_matches(&chars);
    let mut i = 0;
    let mut quote: Option<char> = None;
    macro_rules! end_word { () => {
        if started {
            let w = std::mem::take(&mut word);
            if !redirect && cur.words.is_empty() { cur.hint = format!("{w}{subst_raw}"); }
            else if !redirect && !subst_raw.is_empty() { cur.hint_args.push_str(&subst_raw); cur.hint_args.push('\n'); }
            subst_raw.clear();
            // Past 64 brace groups in one word it is not read (re-review 8 Nov: 19,000 `{,}` took 3.6 GB).
            let all = if brace && w.matches('{').count() > 64 { cur.unreadable = true; vec![w] }
                else if brace { brace_expand(&w).unwrap_or_else(|| { cur.unreadable = true; vec![w.clone()] }) } else { vec![w] };
            if redirect { cur.redirects.extend(all); redirect = false; }
            else { for w in all { cur.words.push(w); cur.expands.push(expands); } }
            started = false; expands = false; brace = false;
        }
    } }
    macro_rules! end_simple { () => { end_word!(); if !cur.words.is_empty() || !cur.redirects.is_empty() { cur.levels = levels.clone(); cur.group = group; out.push(std::mem::take(&mut cur)); } } }
    // A substitution's or a body's commands run one subshell deeper and leave the line as it was.
    let nest = |v: Vec<Simple>, levels: &[(u64, bool)]| {
        let id = new_level();
        let outer = levels.to_vec();
        v.into_iter().map(move |mut s| { let mut l = outer.clone(); l.push((id, true)); l.extend(s.levels); s.levels = l; s })
    };
    while i < chars.len() {
        let c = chars[i];
        match (quote, c) {
            (q, '$') if q != Some('\'') && chars.get(i + 1) == Some(&'(') => {
                // A substitution is its own command line, with its own quotes and here-documents.
                let end = subst_end(&chars, i + 1);
                let inner: String = chars[(i + 2).min(end)..end].iter().collect();
                out.extend(nest(split(&inner), &levels));
                subst_raw.push_str(&inner);
                word.push_str("$(…)");
                expands = true;
                started = true;
                i = end;
            }
            (q, '`') if q != Some('\'') => {
                // A substitution, as `$(…)` is (review 8 Nov: `rm -rf `true` ~/Documents` read as `rm -rf` and a
                // program named ~/Documents): its commands are judged, and the word it stands in is an expansion.
                let mut end = i + 1;
                while end < chars.len() && chars[end] != '`' { if chars[end] == '\\' { end += 1; } end += 1; }
                let inner: String = chars[(i + 1).min(chars.len())..end.min(chars.len())].iter().collect();
                out.extend(nest(split(&inner), &levels));
                subst_raw.push_str(&inner);
                word.push_str("`…`");
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
            // `{` and `}` group commands only as words of their own; `{}` (find's and xargs' placeholder) and `a{b,c}`
            // are words.
            // zsh's `{cd a}`: a group opened without a space, when the braces hold no `,` or `..` of an expansion
            // (seventh review), and the `}` that ends a word closes it.
            (None, '{') if !started && attached_group(&chars, i, &matches) => { end_simple!(); group += 1; attached += 1; }
            (None, '}') if attached > 0 && chars.get(i + 1).is_none_or(|n| n.is_whitespace() || matches!(n, ';' | '&' | '|' | ')')) => {
                end_simple!();
                group = group.saturating_sub(1);
                attached -= 1;
            }
            (None, '$') if chars.get(i + 1) == Some(&'[') => {
                // zsh's `$[ … ]` arithmetic: its `<<` is a shift (seventh review).
                let mut depth = 0usize;
                let mut end = i + 1;
                while end < chars.len() { match chars[end] { '[' => depth += 1, ']' => { depth -= 1; if depth == 0 { break; } } _ => {} } end += 1; }
                word.extend(&chars[i..=end.min(chars.len() - 1)]);
                expands = true;
                started = true;
                i = end;
            }
            (None, '{' | '}') if started || !chars.get(i + 1).is_none_or(|n| n.is_whitespace() || matches!(n, ';' | '&' | '|' | ')')) => {
                if c == '{' { brace = true; }
                word.push(c);
                started = true;
            }
            (None, ';' | '|' | '&' | '(' | ')' | '{' | '}') => {
                // `&>file`, `&>>file`: a redirect of both outputs, not a background job (re-review 8 Nov: `cd x &>/dev/null`).
                if c == '&' && chars.get(i + 1) == Some(&'>') { i += 1; continue; }
                // `2>&1`, `>&2`, `<&-`: a redirect, not a separator.
                if c == '&' && matches!(chars.get(i.wrapping_sub(1)), Some('>' | '<')) {
                    i += 1;
                    let at = i;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '-') { i += 1; }
                    // `>&2`, `2>&1`, `>&-` name a descriptor; `>&file` writes the file (re-review 8 Nov).
                    let fd = i > at && chars.get(i).is_none_or(|n| n.is_whitespace() || matches!(n, ';' | '&' | '|' | ')'));
                    redirect = !fd;
                    if !fd { i = at; }
                    continue;
                }
                let before = out.len();
                end_simple!();
                if out.len() > before {
                    let (next, last) = (chars.get(i + 1), chars.get(i.wrapping_sub(1)));
                    let ends = match c {
                        '&' if next == Some(&'&') => Sep::And,
                        '&' if last != Some(&'&') => Sep::Bg,
                        '|' if next == Some(&'|') => Sep::Or,
                        '|' if last != Some(&'|') => Sep::Pipe,
                        _ => Sep::Seq,
                    };
                    if let Some(l) = out.last_mut() { l.ends = ends; }
                }
                // `(( … ))` is arithmetic, not two subshells; a `(` opens a subshell and its `)` closes it.
                // `name() …` defines a function.
                if c == '(' && chars.get(i + 1) == Some(&')') && out.len() > before { if let Some(l) = out.last_mut() { l.defines_fn = true; } }
                if c == '(' && chars.get(i + 1) == Some(&'(') && cur.words.is_empty() { arith = true; i += 2; continue; }
                if c == ')' && arith && chars.get(i + 1) == Some(&')') { arith = false; i += 2; continue; }
                if c == '{' { group += 1; }
                if c == '}' { group = group.saturating_sub(1); }
                if c == '(' {
                    // Past 64 nested groups the line is not read (sixth review: every command carried all open levels).
                    if levels.len() >= 64 { out.push(Simple { unreadable: true, ..Default::default() }); return out; }
                    let id = new_level();
                    // `NAME=(` opens an array, not a subshell.
                    if chars.get(i.wrapping_sub(1)) == Some(&'=') && out.len() > before { if let Some(l) = out.last_mut() { l.array_level = Some(id); } }
                    levels.push((id, false));
                }
                if c == ')' { levels.pop(); }
                // `a | b` (not `a || b`): b reads a's output.
                cur.after_pipe = c == '|' && chars.get(i + 1) != Some(&'|') && chars.get(i.wrapping_sub(1)) != Some(&'|');
            }
            (None, '\n') => {
                end_simple!();
                // Here-document bodies follow the line that opened them.
                // A shell later in the line reading its input (`cat <<'EOF' | sh`) runs the body (re-review 8 Nov).
                let piped_shell = out[line_start.min(out.len())..].iter().any(|s| s.after_pipe && reads_stdin_shell(&s.words));
                for (doc, shell, id) in std::mem::take(&mut heredocs) {
                    let shell = shell || piped_shell;
                    let mut body = String::new();
                    i += 1;
                    while i < chars.len() {
                        let line_end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..line_end].iter().collect();
                        i = line_end + 1;
                        if doc.ends(&line) { break; }
                        body.push_str(&line);
                        body.push('\n');
                    }
                    if let Some(reader) = out.iter_mut().rev().find(|s| s.docs.contains(&id)) { reader.stdin.push_str(&body); }
                    if shell { out.extend(nest(split(&body), &levels)); }
                    // An unquoted tag's body expands: its substitutions run (re-review 8 Nov: `cat <<EOF` then
                    // `$(rm -rf ~/Documents)` was never judged).
                    else if !doc.quoted { out.extend(nest(body_substitutions(&body), &levels)); }
                    i -= 1;
                }
                line_start = out.len();
            }
            (None, '<') if !arith && chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                end_word!();
                let (doc, next) = Heredoc::read(&chars, i + 2);
                i = next;
                // A shell with no script named reads the body: `sh`, `bash -s`, `zsh -x` (re-review 8 Nov).
                // Its first words say whether it is a shell reading the body (sixth review: copying every word at each `<<`
                // made thousands of here-documents on one command quadratic).
                let shell = reads_stdin_shell(&cur.words[..cur.words.len().min(16)]);
                let id = new_level();
                cur.docs.push(id);
                heredocs.push((doc, shell, id));
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
    // `$(( … ))` is arithmetic: `<<` in it is a shift, never a here-document (re-review 8 Nov).
    let arith = chars.get(open + 1) == Some(&'(');
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut tags: Vec<Heredoc> = vec![];
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') | (None, '\\') => i += 1,
            (Some(_), _) => {}
            (None, '$') if chars.get(i + 1) == Some(&'[') => {
                while i < chars.len() && chars[i] != ']' { i += 1; }
            }
            (None, '$') if chars.get(i + 1) == Some(&'\'') => {
                i += 2;
                while i < chars.len() && chars[i] != '\'' { if chars[i] == '\\' { i += 1; } i += 1; }
            }
            (None, '\'' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => { depth -= 1; if depth == 0 { return i; } }
            (None, '<') if !arith && chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                let (doc, next) = Heredoc::read(&chars, i + 2);
                i = next;
                tags.push(doc);
                continue;
            }
            (None, '\n') => {
                for doc in std::mem::take(&mut tags) {
                    i += 1;
                    while i < chars.len() {
                        let end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = end + 1;
                        if doc.ends(&line) { break; }
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

    /// The user clicked "Allow skill copy" (8 Nov; approvals.log refused update-skills' copy five times): one skill folder
    /// from the Heimdall repository's worker or shared set into `~/.claude/skills/<name>/` or `~/.agents/skills/<name>/`,
    /// with `--delete`, is allowed; nothing broader.
    #[test]
    fn a_skill_copy_into_the_agents_skills_is_allowed_and_nothing_broader() {
        // Not in a temporary folder: those are deletable anyway, and the refusals must come from the skill rule.
        let t = tempfile::Builder::new().prefix(".skill-copy-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let vault = user.join("Work");
        let repo = vault.join("Projects/Heimdall");
        for d in ["skills/workers/share-files", "skills/shared/update-skills", "skills/heimdall/task-skills", "skills/shared/update-skills-new"] {
            std::fs::create_dir_all(repo.join(d)).unwrap();
            std::fs::write(repo.join(d).join("SKILL.md"), "x").unwrap();
        }
        for d in [".claude/skills/share-files", ".agents/skills/share-files", ".claude/skills/update-skills"] { std::fs::create_dir_all(user.join(d)).unwrap(); }
        let (u, r) = (user.display().to_string(), repo.display().to_string());
        let run = |c: &str, cwd: &Path| refuse("Bash", &json!({"command": c}), &vault, &user, Some(cwd));
        for c in [
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete skills/workers/share-files/ {u}/.agents/skills/share-files/"),
            format!("rsync -av --delete {r}/skills/shared/update-skills/ ~/.claude/skills/update-skills/"),
            format!("cd {r}; rsync -a --delete {r}/skills/workers/share-files/ {u}/.claude/skills/share-files/ && rsync -a --delete {r}/skills/workers/share-files/ {u}/.agents/skills/share-files/ && diff -rq skills/workers/share-files {u}/.claude/skills/share-files"),
            format!("rsync -a --delete skills/workers/share-files/ {u}/.agents/skills/share-files/"),
            // Not there yet: rsync makes it.
            format!("rsync -a --delete skills/workers/share-files/ {u}/.agents/skills/update-skills-new/").replace("workers/share-files", "shared/update-skills-new"),
        ] {
            assert_eq!(run(&c, &repo), None, "{c}");
        }
        let refused = [
            // Other destinations.
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/"),
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/share-files/"),
            format!("rsync -a --delete skills/workers/share-files/ {u}/.codex/skills/share-files/"),
            // Another name, another set, another source.
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/hill-climb/"),
            format!("rsync -a --delete skills/heimdall/task-skills/ {u}/.claude/skills/task-skills/"),
            format!("rsync -a --delete {u}/Work/Assistant/Temp/x/skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete /tmp/empty/ {u}/.claude/skills/share-files/"),
            // Escapes and loose forms.
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/../../"),
            format!("rsync -a --delete skills/workers/share-files/../../../ {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete skills/workers/share-files {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files"),
            format!("rsync -a --delete skills/workers/share-files/ skills/workers/x/ {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete --remove-source-files skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete --copy-links skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("rsync -aL --delete skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("rsync -a --delete $SRC/ {u}/.claude/skills/share-files/"),
            format!("H={r}/skills/workers/share-files; rsync -a --delete $H/ {u}/.claude/skills/share-files/"),
        ];
        for c in &refused { assert!(run(c, &repo).is_some(), "{c}"); }
        // Symlinks belong to someone else: a linked skill folder, a link inside it, or a linked skills folder.
        let link = |to: &Path, at: &Path| std::os::unix::fs::symlink(to, at).unwrap();
        let copy = |dst: &str| format!("rsync -a --delete skills/workers/share-files/ {u}/{dst}/share-files/");
        link(&t.path().join("elsewhere"), &user.join(".claude/skills/share-files/linked"));
        assert!(run(&copy(".claude/skills"), &repo).is_some(), "a link inside the skill folder");
        std::fs::remove_file(user.join(".claude/skills/share-files/linked")).unwrap();
        assert_eq!(run(&copy(".claude/skills"), &repo), None);
        std::fs::remove_dir_all(user.join(".agents/skills/share-files")).unwrap();
        link(&t.path(), &user.join(".agents/skills/share-files"));
        assert!(run(&copy(".agents/skills"), &repo).is_some(), "a linked skill folder");
        std::fs::remove_file(user.join(".agents/skills/share-files")).unwrap();
        std::fs::rename(user.join(".agents/skills"), user.join(".agents/real-skills")).unwrap();
        link(&user.join(".agents/real-skills"), &user.join(".agents/skills"));
        assert!(run(&copy(".agents/skills"), &repo).is_some(), "a linked skills folder");
        link(&t.path().join("x"), &repo.join("skills/workers/share-files/linked"));
        assert!(run(&copy(".claude/skills"), &repo).is_some(), "a link in the source");
        std::fs::remove_file(repo.join("skills/workers/share-files/linked")).unwrap();
        assert_eq!(run(&copy(".claude/skills"), &repo), None, "the link gone, the copy passes again");

        // Review 8 Nov: the check runs before the line does, so nothing in the line may plant a link or a source.
        std::fs::create_dir_all(user.join("Documents")).unwrap();
        let line_refused = [
            // A link planted earlier in the same line, then the copy through it: would empty ~/Documents.
            format!("mkdir -p {r}/skills/workers/documents && ln -s {u}/Documents {u}/.agents/skills/documents && rsync -a --delete {r}/skills/workers/documents/ {u}/.agents/skills/documents/"),
            format!("ln -s {u}/Documents {u}/.claude/skills/share-files/docs; rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            // A substitution anywhere in the line, in the copy or beside it.
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/ `ln -s {u}/Library {u}/.claude/skills/x`"),
            format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/ && echo $(ln -s {u}/Library {u}/x)"),
            format!("ls `ln -s {u}/Library {u}/.claude/skills/share-files/l`; rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            // Anything that writes beside it, a redirect included, or a nested shell after one.
            format!("touch x; rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("ls > {u}/.claude/skills/share-files/list; rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/"),
            format!("ln -s {u}/Documents {u}/.claude/skills/share-files/d && sh -c 'rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/'"),
            // A source that is not a skill now: missing, empty, or a link.
            format!("rsync -a --delete skills/workers/never-made/ {u}/.claude/skills/never-made/"),
            format!("rsync -a --delete skills/workers/empty/ {u}/.claude/skills/empty/"),
            format!("rsync -a --delete skills/workers/linked-src/ {u}/.claude/skills/linked-src/"),
        ];
        std::fs::create_dir_all(repo.join("skills/workers/empty")).unwrap();
        std::fs::create_dir_all(user.join(".claude/skills/never-made")).unwrap();
        std::fs::create_dir_all(user.join(".claude/skills/empty")).unwrap();
        link(&repo.join("skills/workers/share-files"), &repo.join("skills/workers/linked-src"));
        for c in &line_refused { assert!(run(c, &repo).is_some(), "{c}"); }
        // Read-only checks beside the copies still pass, as update-skills does them.
        let checked = format!("cd {r} && ls -ld {u}/.claude/skills && test -d skills/workers/share-files && find {u}/.claude/skills/share-files -type l && rsync -a --delete {r}/skills/workers/share-files/ {u}/.claude/skills/share-files/ && diff -rq skills/workers/share-files {u}/.claude/skills/share-files");
        assert_eq!(run(&checked, &user), None, "{checked}");
        // After a `cd` the gate does not know the folder (it may have failed): a relative source is refused there.
        let relative = format!("cd {r} && rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/");
        assert!(run(&relative, &user).is_some(), "{relative}");
    }

    /// Re-check 8 Nov: a skill copy's line may hold no assignment, wrapper, `$`, path-named "read-only" program, `cd`
    /// flag or process substitution; each could make the gate's reading of the paths differ from the shell's.
    #[test]
    fn a_skill_copy_line_holds_nothing_the_shell_reads_differently() {
        let t = tempfile::Builder::new().prefix(".skill-line-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let vault = user.join("Work");
        let repo = vault.join("Projects/Heimdall");
        std::fs::create_dir_all(repo.join("skills/workers/share-files")).unwrap();
        std::fs::write(repo.join("skills/workers/share-files/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(user.join(".claude/skills/share-files")).unwrap();
        let (u, r) = (user.display().to_string(), repo.display().to_string());
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &vault, &user, Some(&repo));
        let copy = format!("rsync -a --delete skills/workers/share-files/ {u}/.claude/skills/share-files/");
        assert_eq!(run(&copy), None, "the copy alone passes");
        assert_eq!(run(&format!("ls && test -d skills && {copy}")), None, "beside bare read-only commands");
        let full = format!("rsync -a --delete {r}/skills/workers/share-files/ {u}/.claude/skills/share-files/");
        assert_eq!(run(&format!("cd {r} && ls && {full}")), None, "after a cd, with the source named in full");
        // Re-review 8 Nov: zsh keeps none of these `cd`s, or may not; a copy relative to the folder, or into `./`,
        // would then reach the session's own folder.
        std::fs::create_dir_all(user.join(".claude/skills/new-skill")).unwrap();
        let dot = format!("rsync -a --delete {r}/skills/workers/share-files/ ./");
        for c in [
            format!("cd {u}/.claude/skills/share-files; {dot}"),
            format!("cd {u}/.claude/skills/never-made; {copy}"),
            format!("[ -d {u}/x ] && cd {u}/.claude/skills/share-files; {dot}"),
            format!("true || cd {u}/.claude/skills/share-files; {dot}"),
            format!("cd () {{ true; }}; cd {u}/.claude/skills/share-files; {dot}"),
            format!("go () {{ cd {u}; }}; {copy}"),
            format!("{{ cd {u}/.claude/skills/share-files; }}; {dot}"),
        ] {
            assert!(run(&c).is_some(), "{c}");
        }
        for before in [
            "HOME=/tmp/x".to_string(), "CDPATH=/tmp/x".into(), "PATH=/tmp/x".into(), "HOME=/tmp/x cd".into(),
            "env HOME=/tmp/x ls".into(), "command ls".into(), "exec ls".into(), "nice -n 19 ls".into(), "nohup ls".into(),
            "ls ${CDPATH:=/somewhere}".into(), "ls $HOME".into(), "test -d $PWD".into(),
            "./ls".into(), "/tmp/x/ls".into(), format!("{r}/ls"),
            "cd -P skills".into(), "cd -- skills".into(), "cd skills extra".into(),
            "diff <(ls) <(ls)".into(), "ls >(cat)".into(),
        ] {
            let c = format!("{before}; {copy}");
            assert!(run(&c).is_some(), "{c}");
        }
        for wrapped in [format!("HOME=/tmp/x {copy}"), format!("env {copy}"), format!("nice -n 19 {copy}"), format!("command {copy}"), format!("{copy} <(ls)")] {
            assert!(run(&wrapped).is_some(), "{wrapped}");
        }
    }

    /// The user's "Allow it" (8 Nov ~05:10): update-skills' copy into Heimdall's own skills folder,
    /// `~/Library/Application Support/Heimdall/agent/skills/<name>/`, from the repository's `heimdall/` or `shared/`
    /// set, and the chained form approvals.log refused (line 109). Nothing broader, and the known bypasses still refused.
    #[test]
    fn a_skill_copy_into_heimdalls_own_skills_is_allowed_and_nothing_broader() {
        let t = tempfile::Builder::new().prefix(".skill-heimdall-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let vault = user.join("Work");
        let repo = vault.join("Projects/Heimdall");
        for d in ["skills/heimdall/task-skills", "skills/shared/update-skills", "skills/workers/share-files", "skills/public/task-skills"] {
            std::fs::create_dir_all(repo.join(d)).unwrap();
            std::fs::write(repo.join(d).join("SKILL.md"), "x").unwrap();
        }
        let hs = user.join("Library/Application Support/Heimdall/agent/skills");
        for d in ["task-skills", "update-skills"] { std::fs::create_dir_all(hs.join(d)).unwrap(); }
        std::fs::write(hs.join(".seeded"), "task-skills\n").unwrap();
        std::fs::create_dir_all(user.join(".claude/skills/share-files")).unwrap();
        std::fs::create_dir_all(user.join(".agents/skills/share-files")).unwrap();
        let (u, r) = (user.display().to_string(), repo.display().to_string());
        let h = format!("\"{u}/Library/Application Support/Heimdall/agent/skills");
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &vault, &user, Some(&repo));
        let copy = format!("rsync -a --delete skills/heimdall/task-skills/ {h}/task-skills/\"");
        let allowed = [
            copy.clone(),
            format!("rsync -a --delete {r}/skills/shared/update-skills/ {h}/update-skills/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ ~/\"Library/Application Support/Heimdall/agent/skills/task-skills/\""),
            format!("rsync -av --delete skills/heimdall/task-skills/ {u}/Library/Application\\ Support/Heimdall/agent/skills/task-skills/"),
            // approvals.log line 109, whole: one `--delete` copy into Heimdall's folder, plain copies into the agents'.
            format!("rsync -a --delete skills/heimdall/task-skills/ {h}/task-skills/\" && rsync -a skills/workers/share-files/ {u}/.claude/skills/share-files/ && rsync -a skills/workers/share-files/ {u}/.agents/skills/share-files/"),
            format!("cd {r} && ls -ld {h}\" && rsync -a --delete {r}/skills/heimdall/task-skills/ {h}/task-skills/\" && diff -rq skills/heimdall/task-skills {h}/task-skills\""),
        ];
        for c in &allowed { assert_eq!(run(c), None, "{c}"); }
        let refused = [
            // The folder itself, its parents, a sibling of it, another name.
            format!("rsync -a --delete skills/heimdall/task-skills/ {h}/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ \"{u}/Library/Application Support/Heimdall/agent/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ \"{u}/Library/Application Support/Heimdall/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ \"{u}/Library/Application Support/Heimdall/agent/config/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ {h}/update-skills/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ {h}/.seeded/\""),
            // A set Heimdall's folder does not load, and Heimdall's set into the agents' folders.
            format!("rsync -a --delete skills/workers/share-files/ {h}/share-files/\""),
            format!("rsync -a --delete skills/public/task-skills/ {h}/task-skills/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ {u}/.agents/skills/task-skills/"),
            // Another source, escapes, loose forms.
            format!("rsync -a --delete {u}/Work/Assistant/Temp/skills/heimdall/task-skills/ {h}/task-skills/\""),
            format!("rsync -a --delete /tmp/empty/ {h}/task-skills/\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ {h}/task-skills/../\""),
            format!("rsync -a --delete skills/heimdall/task-skills/ {h}/task-skills\""),
            format!("rsync -a --delete skills/heimdall/task-skills {h}/task-skills/\""),
            format!("rsync -a --delete --copy-links skills/heimdall/task-skills/ {h}/task-skills/\""),
            // A plain copy beside the delete that is not itself the skill copy could plant a link the delete follows.
            format!("rsync -a /tmp/x/ {h}/\" && {copy}"),
            format!("rsync -a skills/workers/share-files/ {u}/.claude/skills/ && {copy}"),
            format!("{copy} && rsync -a /tmp/x/ {u}/.claude/skills/share-files/"),
            // The known bypasses, before the copy or wrapping it.
            format!("HOME=/tmp/x; {copy}"), format!("HOME=/tmp/x {copy}"), format!("env {copy}"), format!("nice -n 19 {copy}"),
            format!("CDPATH=/tmp/x; {copy}"), format!("ls $HOME; {copy}"), format!("ls ${{CDPATH:=/x}}; {copy}"),
            format!("./ls; {copy}"), format!("/tmp/x/ls; {copy}"), format!("cd -P skills; {copy}"),
            format!("{copy} `ln -s {u}/Documents {h}/task-skills/d\"`"), format!("ls $(ln -s {u}/Documents {u}/x); {copy}"),
            format!("diff <(ls) <(ls); {copy}"), format!("ln -s {u}/Documents {h}/docs\"; {copy}"),
            format!("mkdir -p {h}/x\" && {copy}"), format!("ls > {h}/task-skills/l\"; {copy}"),
            format!("sh -c 'ln -s {u}/Documents {h}/task-skills/d\"'; {copy}"),
        ];
        for c in &refused { assert!(run(c).is_some(), "{c}"); }
        // A link on the way to Heimdall's folder or inside the skill belongs to someone else.
        let link = |to: &Path, at: &Path| std::os::unix::fs::symlink(to, at).unwrap();
        link(&user.join("Documents"), &hs.join("task-skills/linked"));
        assert!(run(&copy).is_some(), "a link inside the skill folder");
        std::fs::remove_file(hs.join("task-skills/linked")).unwrap();
        assert_eq!(run(&copy), None, "the link gone, the copy passes again");
        let agent = user.join("Library/Application Support/Heimdall/agent");
        std::fs::rename(&hs, agent.join("real-skills")).unwrap();
        link(&agent.join("real-skills"), &hs);
        assert!(run(&copy).is_some(), "a linked skills folder");
        std::fs::remove_file(&hs).unwrap();
        std::fs::rename(agent.join("real-skills"), &hs).unwrap();
        std::fs::remove_dir_all(hs.join("task-skills")).unwrap();
        link(&user.join("Documents"), &hs.join("task-skills"));
        assert!(run(&copy).is_some(), "a linked skill folder");
    }

    /// Review 8 Nov: an unquoted backtick is a substitution, as `$(` is. `rm -rf `true` ~/Documents` was read as
    /// `rm -rf` and a program named ~/Documents, and passed.
    #[test]
    fn backticks_are_substitutions() {
        for c in ["rm -rf `true` /Users/t/Documents", "rm -rf `echo` ~/Documents", "true `rm -rf /Users/t/Documents`", "echo \"`rm -rf ~/Library`\""] {
            assert!(bash(c).is_some(), "{c}");
        }
        assert_eq!(bash("rm -rf /Users/t/Work/Assistant/Temp/x `true`"), None, "a substitution beside an allowed delete");
        assert_eq!(bash("git log -1 --format=`echo %h`"), None, "ordinary work");
    }

    /// Re-review 8 Nov: the words the shell runs, read as it reads them. A leading substitution that expands to nothing,
    /// brace expansion, a here-document's quoted tag with a space and an unquoted body's substitutions, and redirects
    /// with no program or as `&>`/`>&`.
    #[test]
    fn the_gate_reads_words_as_the_shell_does() {
        for c in [
            "`true` rm -rf ~/Documents", "nice `true` rm -rf ~/Documents", "$(true) rm -rf ~/Documents", "$X rm -rf ~/Documents",
            "command {rm,-rf} ~/Documents", "{rm,-rf} ~/Documents", "rm -rf ~/{Documents,Work/Assistant/Temp/x}",
            "find ~/Documents -delete{,}", "find ~/Documents -{delete,print}",
            "ls <<'EO F'\nx\nEO F\nrm -rf ~/Documents", "cat <<\"A B\"\nx\nA B\nrm -rf ~/Documents",
            "cat <<EOF\n$(rm -rf ~/Documents)\nEOF", "cat <<EOF\n`rm -rf ~/Documents`\nEOF",
            "> ~/Library/LaunchAgents/x.plist", "echo x &> ~/Library/LaunchAgents/x.plist", "echo x >& ~/Library/LaunchAgents/x.plist",
            "echo x >&/Users/t/Library/LaunchAgents/x.plist",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "cat <<'EOF'\n$(rm -rf ~/Documents)\nEOF", "cat <<\\EOF\n`rm -rf ~/Documents`\nEOF",
            "python3 - <<'PY'\nprint('rm -rf ~/Documents')\nPY", "cat <<EOF\nrm -rf ~/Documents\nEOF",
            "echo x 2>&1", "echo x >&2", "ls 2>&- ", "echo {a,b}", "find . -name '*.rs' -exec grep -l x {} +",
            "rm -rf /Users/t/Work/Assistant/Temp/{a,b}", "$PY script.py /Users/t/Library/Logs/x.log", "\"$VENV/bin/python\" x.py",
            "rsync -a --delete --exclude .git /Users/t/Documents/proj/ /Users/t/Work/Projects/X/",
            "rsync -a --delete --exclude .git /Users/t/Documents/proj/ /Users/t/Work/Projects/X/ 2>&1",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        assert!(bash("rsync -a --delete /Users/t/Work/Projects/X/ /Users/t/Documents/proj/").is_some(), "the last operand is still the target");
        assert!(bash("rsync -a --delete /Users/t/Work/Projects/X/ /Users/t/Documents/proj/ `true`").is_some(), "every operand with an expansion");
        assert_eq!(brace_expand("a{b,c{d,e}}f").unwrap(), ["abf", "acdf", "acef"]);
        assert_eq!(brace_expand("{}").unwrap(), ["{}"]);
        assert_eq!(brace_expand("${HOME}").unwrap(), ["${HOME}"]);
        assert_eq!(brace_expand("find{,}{,}{,}").unwrap(), ["find"], "repeats kept once");
    }

    /// Second re-review 8 Nov: a `cd` that may not hold is judged from both folders; rsync's real target with options
    /// after the paths; a program named by an expansion; here-document ends and shells that read a body.
    #[test]
    fn a_cd_that_may_not_hold_and_other_second_review_cases() {
        let home = |c: &str| bash_in(c, "/Users/t");
        for c in [
            "cd /tmp/never-made; rm -rf *", "(cd /tmp/x); rm -rf build", "cd /tmp/x || true; rm -rf build",
            "$X cd /tmp; rm -rf build", "cat <<EOF\n$(cd /tmp)\nEOF\nrm -rf build", "cd /tmp/x | true; rm -rf build",
            "cd /tmp/x && true; rm -rf build", "cd /tmp/x & rm -rf build",
        ] {
            assert!(home(c).is_some(), "{c:?}");
        }
        for c in ["cd /tmp/x && rm -rf build", "cd /tmp/x && rm -rf build && ls", "cd /Users/t/Work/Projects/X && rm -rf target; ls",
                  "cd /tmp/x && rm -rf build || echo failed"] {
            assert_eq!(home(c), None, "{c:?}");
        }
        assert_eq!(bash("cd /tmp/x; rm -rf build"), None, "both folders allow it: /tmp/x and the Vault");
        for c in [
            "rsync -a --delete /Users/t/Work/Projects/X/ /Users/t/Documents/proj/ --exclude .git",
            "rsync -a --delete /Users/t/Work/Projects/X/ /Users/t/Documents/proj/ --log-file /tmp/x.log",
            "rsync -a --delete -e ssh /Users/t/Work/Projects/X/ /Users/t/Documents/proj/ --exclude node_modules",
            "rsync -a --remove-source-files /Users/t/Documents/proj/ /tmp/x/",
            "$(which rm) -rf ~/Documents", "`which rm` -rf ~/Documents", "$RM -rf ~/Documents", "$SHELL -c 'rm -rf ~/Documents'",
            "find{,}{,}{,}{,}{,}{,}{,}{,}{,} ~/Documents -delete",
            "cat <<EOF\n  EOF\n'\nEOF\nrm -rf ~/Documents", "bash -s <<'EOF'\nrm -rf ~/Documents\nEOF",
            "cat <<'EOF' | sh\nrm -rf ~/Documents\nEOF", "cat <<-EOF\n\tEOF\nrm -rf ~/Documents",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "rsync -a --delete --exclude .git /Users/t/Documents/proj/ /Users/t/Work/Projects/X/",
            "rsync -a --delete /Users/t/Documents/proj/ /Users/t/Work/Projects/X/ --exclude .git",
            "rsync -ae ssh --delete /Users/t/Documents/proj/ /Users/t/Work/Projects/X/",
            "rsync -a /Users/t/Documents/proj/ /tmp/x/", "\"$VENV/bin/python\" -m pytest -q /Users/t/Library/x",
            "$PY -c 'print(1)'", "python3 - <<'PY'\nimport shutil\nprint('rm -rf ~/Documents')\nPY",
            "cat <<'EOF' > /tmp/x.sh\nrm -rf ~/Documents\nEOF",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
    }

    /// Third review 8 Nov: the folder states in linear time, `||` and pipelines in the `&&` logic, rsync attached
    /// option values, removers and shells named by an expansion, large brace expansions, arithmetic `<<`.
    #[test]
    fn third_review_cases() {
        let home = |c: &str| bash_in(c, "/Users/t");
        let vault_root = |c: &str| bash_in(c, "/Users/t/Work");
        // A long line of cds stays fast, and a refusal early in it is still found.
        let many: String = (0..80).map(|n| format!("cd /tmp/a{n}; ls; ")).collect();
        let t = std::time::Instant::now();
        assert_eq!(home(&many), None);
        assert!(home(&format!("cd ~/Documents && rm -rf *; {many}")).is_some());
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "{:?}", t.elapsed());
        for c in [
            "rsync -a --delete -f'- .DS_Store' /Users/t/Work/Projects/X/ /Users/t/Documents/proj/",
            "rsync -a --delete -f'exclude .cache' /Users/t/Work/Projects/X/ /Users/t/Documents/proj/",
            "rsync -a --del /Users/t/Work/Projects/X/ /Users/t/Documents/proj/",
            "$SHELL -lc 'rm -rf ~/Documents'", "\"$SHELL\" -ec 'rm -rf ~/Documents'",
            "bash -s -- x <<'EOF'\nrm -rf ~/Documents\nEOF", "cat <<'EOF' | bash -s -- x\nrm -rf ~/Documents\nEOF", "$SHELL <<'EOF'\nrm -rf ~/Documents\nEOF",
            "$RM --recursive --force ~/Documents", "$RM -rfP ~/Documents", "${RM_CMD} ~/Documents",
            "(( n = 1 << 4 ))\nrm -rf ~/Documents", "echo $(( 1 << 4 ))\nrm -rf ~/Documents",
            "eval cd ~/Documents; rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        let big: String = format!("rm -rf {{{}/Users/t/Documents}}", "a,".repeat(300));
        assert!(bash(&big).is_some(), "past 1,024 words or not, the Documents target is seen");
        assert!(bash(&"x{a,b}".repeat(24)).is_some(), "a brace expansion too large to read is refused");
        for c in [
            "cd ~/Documents || cd /tmp/x && rm -rf *", "test -e x || cd /tmp/x && rm -rf build",
            "cd -P ~/Documents && rm -rf *", "cd -- ~/Documents && rm -rf *", "pushd ~/Documents && rm -rf *",
            "chdir ~/Documents && rm -rf *", "cd - && rm -rf build", "popd; rm -rf build",
        ] {
            assert!(home(c).is_some(), "{c:?}");
        }
        for c in [
            "cd /tmp/x && find . -name '*.pyc' | xargs rm -f", "cd /Users/t/Work/Projects/X && git ls-files -d | xargs rm",
            "cd /tmp/x && ls | xargs rm -rf", "cd /tmp/x && rm -rf build/$(date +%s)", "cd /tmp/x; cd /tmp/y && rm -rf build",
        ] {
            assert_eq!(home(c), None, "{c:?}");
        }
        // A cd to an expansion goes to a folder the gate cannot know (seventh review); naming it in full passes.
        assert!(home("cd \"$WT\" && rm -rf target").is_some());
        assert_eq!(home("rm -rf \"$WT/target\""), None);
        for c in ["$GREP -r foo ~/Library/Logs", "$PIP install -r ~/Documents/req.txt", "$PY -m pytest -q", "echo $(( 1 << 4 ))"] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        // From the Vault root a cd that fails leaves the delete one folder deep: refused, and `&&` is the way.
        assert!(vault_root("cd Projects/X; rm -rf build").is_some());
        assert_eq!(vault_root("cd Projects/X && rm -rf build"), None);
    }

    /// Fourth review 8 Nov: redirects that look like `&`, subshells told apart, `!`, `exit`, the deadline, and older
    /// folder changes (two-word cd, `~-`, `~name`, CDPATH, loops, functions, deep evals, a link out of the Vault).
    #[test]
    fn fourth_review_cases() {
        let home = |c: &str| bash_in(c, "/Users/t");
        let root = |c: &str| bash_in(c, "/Users/t/Work");
        for c in [
            "cd /Users/t/Documents &>/dev/null && rm -rf *", "cd /Users/t/Documents &>>log; rm -rf *", "cd /Users/t/Documents <&-; rm -rf *",
            "cd /Users/t; cd /Users/t/Work/Projects/Nope && ( echo $(true) ) || rm -rf Documents",
            "! cd /Users/t || rm -rf Documents",
            "cd /Users/t/Work/Projects/Heimdall /Users/t/Documents && rm -rf *", "cd ~- && rm -rf *", "cd ~t && rm -rf *",
            "CDPATH=/Users/t cd Documents && rm -rf *", "export CDPATH=/Users/t; cd Documents && rm -rf *", "cdpath=(/Users/t); cd Documents && rm -rf *",
            "for i in 1 2 3 4 5; do cd ..; done; rm -rf Documents", "repeat 5 cd ..; rm -rf Documents",
            "f() { cd /Users/t; }; cd /Users/t/Work/Projects/X && f && rm -rf Documents",
            "eval eval eval eval eval rm -rf /Users/t/Documents",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        assert!(home("cd /Users/t/Work/Projects/Heimdall && (cd \"$(git rev-parse --show-toplevel)\" && make) || rm -rf Documents").is_some());
        for c in [
            "cd /Users/t/Work/Projects/Heimdall && cd crates &>/dev/null && rm -rf *",
            "cd /Users/t/Work/Projects/Heimdall && ( rm -rf target )", "cd /Users/t/Work/Projects/Heimdall && (cd crates && rm -rf target)",
            "cd /Users/t/Work/Projects/X || exit 1\nrm -rf build",
        ] {
            assert_eq!(home(c), None, "{c:?}");
        }
        // An `exit` in a group may not run (fifth review): only a plain line's `exit` ends a path, so this is refused.
        assert!(home("cd /Users/t/Work/Projects/X || { echo no; exit 1; }; rm -rf build").is_some());
        assert_eq!(root("cd Projects/Heimdall && ( rm -rf target )"), None);
        // Lines that multiply work finish in time, refused or not.
        let cds: String = (0..8).map(|n| format!("cd d{n}; ")).collect();
        let rms: String = (0..10).map(|n| format!("rm -rf b{n}; ")).collect();
        let inner = format!("{cds}{rms}");
        for c in [format!("{cds}bash -c '{inner}'"), format!("{cds}bash -c \"{cds}bash -c '{inner}'\""), format!("{cds}{}rm -rf /Users/t/Documents", "rm -rf bx; ".repeat(1000))] {
            let t = std::time::Instant::now();
            let _ = bash(&c);
            assert!(t.elapsed() < std::time::Duration::from_secs(6), "{:?} for {} chars", t.elapsed(), c.len());
        }
        assert!(bash(&format!("{cds}{}rm -rf /Users/t/Documents", "rm -rf bx; ".repeat(1000))).is_some());
        // A link in the Vault to a folder outside it: deleting into it is deleting there.
        let t = tempfile::Builder::new().prefix(".link-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let vault = user.join("Work");
        let project = vault.join("Projects/H");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(user.join("Documents")).unwrap();
        std::os::unix::fs::symlink(user.join("Documents"), project.join("link")).unwrap();
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &vault, &user, Some(&project));
        let l = project.join("link").display().to_string();
        for c in [format!("cd {l} && rm -rf *"), format!("rm -rf {l}/*"), format!("rm -rf {l}/"), "rm -rf link/*".to_string()] {
            assert!(run(&c).is_some(), "{c}");
        }
        assert_eq!(run(&format!("rm -rf {l}")), None, "the link itself");
        assert_eq!(run("rm -rf build/*"), None);
    }

    /// Fifth review 8 Nov: `exit` that may not run, zsh's short loops, parse limits, `()` and CDPATH only where they
    /// define or set something.
    #[test]
    fn fifth_review_cases() {
        for c in [
            "[ -d /x ] || { echo missing; exit 1; }; rm -rf /Users/t/Documents",
            "if [ -z \"$X\" ]; then echo need X; exit 1; fi; rm -rf /Users/t/Documents",
            "if true; then echo ok; else exit 1; fi; rm -rf /Users/t/Documents",
            "f() { exit 1; }; rm -rf /Users/t/Documents", "f() { return 1; }; rm -rf /Users/t/Documents",
            "die() { echo \"$1\" >&2; exit 1; }\nrm -rf /Users/t/Documents", "exit & rm -rf /Users/t/Documents",
            "case $x in a) exit;; esac; rm -rf /Users/t/Documents", "while false; do exit; done; rm -rf /Users/t/Documents",
            "for i (1 2 3) cd ..; rm -rf Documents", "for i in 1 2 3; cd ..; rm -rf Documents", "for i in 1 2 3; { cd .. }; rm -rf Documents",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "cd /Users/t/Work/Projects/Heimdall && rm -rf build && python3 -c \"import m; m.main()\"",
            "cd /Users/t/Work/Projects/Heimdall && rg -n \"fn main()\" src; rm -rf target/debug",
            "cd /Users/t/Work/Projects/Heimdall && python3 - <<'EOF'\ndef f():\n    pass\nEOF\nrm -rf build",
            "cd /Users/t/Work/Projects/Heimdall/crates && find . -name '*.pyc' -delete && node -e 'main()'",
            "cd /Users/t/Work/Projects/Heimdall && rm -rf dist && npm run build -- --define 'f()'",
            "cd /Users/t/Work/Projects/Heimdall && rm -rf build; for d in a b; do (cd \"$d\" && make); done",
            "grep -n CDPATH ~/.zshrc; cd crates && rm -rf target",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        // Parsing is bounded: deep nesting, huge brace words and many expansions finish fast and are refused or read.
        let t = std::time::Instant::now();
        assert!(bash(&format!("{}rm -rf /Users/t/Documents{}", "echo $(".repeat(4000), ")".repeat(4000))).is_some());
        assert!(bash(&format!("echo {}", "{,}".repeat(19_000))).is_some());
        let _ = bash(&"$x ".repeat(20_000));
        assert!(t.elapsed() < std::time::Duration::from_secs(3), "{:?}", t.elapsed());
    }

    /// Sixth review 8 Nov: huge here-document and nesting lines bounded, a loop's subshell cd, pushd/popd pairs.
    #[test]
    fn sixth_review_cases() {
        let t = std::time::Instant::now();
        let docs = format!("cat {}\n{}", "a <<E ".repeat(15_000), "x\nE\n".repeat(15_000));
        let _ = bash(&docs);
        assert!(bash(&format!("{}ls; rm -rf /Users/t/Documents", "( ".repeat(20_000))).is_some());
        assert!(t.elapsed() < std::time::Duration::from_secs(3), "{:?}", t.elapsed());
        for c in [
            "for d in a b; do (cd \"$d\" && make clean); done && rm -rf build",
            "pushd crates >/dev/null && cargo build && popd >/dev/null; rm -rf build",
            "pushd /tmp/x && rm -rf y && popd; rm -rf build",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        // A pushd that may fail leaves the later popd nothing saved: it fails where it is (each line runs in a fresh
        // shell, so the stack starts empty).
        assert_eq!(bash("pushd /tmp/x && rm -rf y; popd; rm -rf build"), None);
        for c in [
            "pushd /Users/t/Documents && popd && pushd /Users/t && rm -rf Documents",
            "pushd /Users/t/Documents && popd && pushd /Users/t && popd && pushd /Users/t && rm -rf Documents/x",
            "for d in a b; do cd \"$d\"; done; rm -rf build",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
    }

    /// Seventh review 8 Nov (lines run through zsh against the folder model): expansions in cd, zsh's `{cd a}`, `cd -P`
    /// through a link, zsh's short `if`, `$[…]`, `source`, loops after keywords, `!` after `then`, `exit` behind a
    /// wrapper, `nocorrect`; and ordinary lines that were refused.
    #[test]
    fn seventh_review_cases() {
        let docs = |c: &str| bash_in(c, "/Users/t/Documents");
        for c in [
            "cd \"$HOME/Documents\" && rm -rf *", "cd $HOME && rm -rf Documents", "D=~/Documents; cd $D && rm -rf *",
            "cd \"$PWD/a\" && rm -rf /x/../*", "cd $(echo /Users/t) && rm -rf Documents",
            "{cd ~/Documents}; rm -rf *", "{cd ~/Documents;} && rm -rf *",
            "if [[ -d x ]] cd ../../..; rm -rf *", "echo $[1<<2]\nrm -rf ~/Documents", ". ./go.sh && rm -rf *",
            "if true; then for i in 1 2; do cd ..; done; fi; rm -rf *",
            "for i in 1; do echo; done; (for j in 1 2; do cd ..; done; rm -rf *)",
            "nocorrect cd ~/Documents; rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in ["if true; then ! cd /tmp/never-made && rm -rf *; fi", "cd /tmp/never-made || command exit 1; rm -rf *", "cd /tmp/never-made || env exit 1; rm -rf *"] {
            assert!(docs(c).is_some(), "{c:?}");
        }
        for c in [
            "pushd x; make; popd; rm -rf build",
            "for f in *.txt; do echo $f; done; cd x && rm -rf build",
            "f() { make; }; cd x && f && rm -rf build",
            "cd x && for d in a b; do echo $d; done && cd .. && rm -rf build",
            "cd \"$HOME/Work/Projects/X\" && rm -rf build", "echo {a,b} {1..3} {}", "find . -name x -exec echo {} +",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        // `cd -P` resolves a link in the Vault to a folder outside it.
        let t = tempfile::Builder::new().prefix(".cdp-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let vault = user.join("Work");
        let project = vault.join("Projects/H");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(user.join("Documents/sub")).unwrap();
        std::os::unix::fs::symlink(user.join("Documents/sub"), project.join("docs")).unwrap();
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &vault, &user, Some(&project));
        assert!(run("cd -P docs/.. && rm -rf *").is_some());
        assert!(run("cd docs && cd -P .. && rm -rf *").is_some());
        assert_eq!(run("cd docs/.. && rm -rf build"), None, "logical: back in the project");
    }

    /// Eighth review 8 Nov (zsh comparison rerun): loops with `(( … ))` or several-command conditions, zsh groups
    /// with commas, shell options and HOME set in the line; `source` of a file without a cd passes.
    #[test]
    fn eighth_review_cases() {
        let deep = |c: &str| bash_in(c, "/Users/t/Work/Projects/Heimdall/crates/app");
        for c in [
            "for (( i=0; i<5; i++ )); do cd ..; done; rm -rf *", "i=0; while (( i++ < 5 )); do cd ..; done; rm -rf *",
            "i=0; while (( i++ < 5 )) cd ..; rm -rf *", "until (( i++ > 5 )) cd ..; rm -rf *",
            "while i=$((i+1)); [ $i -le 5 ]; do cd ..; done; rm -rf *", "if true; then while (( i < 5 )); do cd ..; done; fi; rm -rf *",
        ] {
            assert!(deep(c).is_some(), "{c:?}");
        }
        for c in [
            "{cd ~/Documents; echo x,y}; rm -rf *", "{cd ~/Documents; echo done, ok}; rm -rf *", "{cd ~/Documents; echo x..y}; rm -rf *",
            "{echo {x,y}; cd ~/Documents}; rm -rf *", "setopt chaselinks; cd docs/.. && rm -rf *", "setopt autopushd; cd ~/Documents && cd /tmp && popd && rm -rf *",
            "HOME=/Users/t/Documents; cd && rm -rf *", "HOME=/Users/t/Documents cd ~ && rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        // A sourced file is never read (tenth review): the folder after it is unknown, so a relative delete is refused;
        // the delete named in full passes.
        let t = tempfile::Builder::new().prefix(".source-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let project = user.join("Work/Projects/H");
        std::fs::create_dir_all(project.join(".venv/bin")).unwrap();
        std::fs::write(project.join(".venv/bin/activate"), "export VIRTUAL_ENV=x\nhash -r 2>/dev/null\n").unwrap();
        std::fs::write(project.join("go.sh"), "cd /Users/t/Documents\n").unwrap();
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &user.join("Work"), &user, Some(&project));
        assert!(run("source .venv/bin/activate && rm -rf build").is_some());
        assert!(run(". .venv/bin/activate; pytest; rm -rf build").is_some());
        assert_eq!(run(&format!("source .venv/bin/activate && rm -rf {}", project.join("build").display())), None);
        assert!(run(". ./go.sh && rm -rf *").is_some());
        assert!(run("source ./missing.sh && rm -rf *").is_some());
    }

    /// Ninth review 8 Nov (zsh comparison, third run): a sourced file is read only when it is safe to (an ordinary
    /// file, at the start of the line, sourcing nothing itself); options anywhere in a command; more HOME forms.
    #[test]
    fn ninth_review_cases() {
        let t = tempfile::Builder::new().prefix(".source2-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let project = user.join("Work/Projects/H");
        std::fs::create_dir_all(project.join(".venv/bin")).unwrap();
        std::fs::create_dir_all(user.join("Documents")).unwrap();
        std::fs::write(project.join(".venv/bin/activate"), "export VIRTUAL_ENV=x\n").unwrap();
        std::fs::write(project.join("go.sh"), format!("cd {}\n", user.join("Documents").display())).unwrap();
        std::fs::write(project.join("go2.sh"), ". ./go.sh\n").unwrap();
        std::fs::write(project.join("w.sh"), "echo hi\n").unwrap();
        assert!(std::process::Command::new("mkfifo").arg(project.join("fifo")).status().unwrap().success());
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &user.join("Work"), &user, Some(&project));
        let started = std::time::Instant::now();
        for c in [
            ". ./fifo & rm -rf ~/Documents/x", ". ./fifo; rm -rf *", "source /dev/zero; rm -rf *", "source /dev/stdin <<< 'cd ~'; rm -rf *",
            ". ./go2.sh && rm -rf *", "echo 'cd ~/Documents' > w.sh && . ./w.sh && rm -rf *", "true; . ./w.sh && rm -rf *",
            "{source ./go.sh; echo x,y}; rm -rf *", "{source ./go2.sh}; rm -rf *",
        ] {
            assert!(run(c).is_some(), "{c:?}");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "a pipe is never read");
        assert!(run("source .venv/bin/activate && rm -rf build").is_some());
        for c in [
            "then setopt chaselinks; cd docs/.. && rm -rf *", "builtin setopt chaselinks; cd docs/.. && rm -rf *",
            "noglob setopt chaselinks; cd docs/.. && rm -rf *", "set -w; cd docs/.. && rm -rf *", "set -N; cd /Users/t/Documents && cd /tmp && popd && rm -rf *",
            "options[chaselinks]=on; cd docs/.. && rm -rf *", "set -o autopushd; cd /Users/t/Documents && cd /tmp && popd && rm -rf *",
            "HOME+=/Documents; cd && rm -rf *", ": ${HOME::=/Users/t/Documents}; cd && rm -rf *", "read HOME <<< /Users/t/Documents; cd && rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in ["set -o pipefail; cd x && rm -rf build", "set -euo pipefail; cd x && rm -rf build", "jq '.x | {cd: .y}' f.json; rm -rf build"] {
            assert_eq!(bash(c), None, "{c:?}");
        }
    }

    /// Tenth review 8 Nov (zsh comparison, fourth run): sources never read, CDPATH and options set any way, linear
    /// brace matching, `PYTHONHOME=` is not HOME.
    #[test]
    fn tenth_review_cases() {
        for c in [
            ". ./w.sh =(echo 'cd ~/Documents' > w.sh) && rm -rf *", ". ./w.sh <<EOF\nEOF\nrm -rf *",
            ": ${CDPATH:=$HOME}; cd Documents && rm -rf *", "cdpath[1]=~; cd Documents && rm -rf *", "read CDPATH <<< ~; cd Documents && rm -rf *",
            "setopt $opt; cd docs/.. && rm -rf *", "options+=(chaselinks on); cd docs/.. && rm -rf *", "setopt -w; cd docs/.. && rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        assert_eq!(bash("export PYTHONHOME=/x; cd /Users/t/Work/Projects/P && rm -rf build"), None);
        assert_eq!(bash("grep -n CDPATH ~/.zshrc; cd crates && rm -rf target"), None);
        let t = std::time::Instant::now();
        let _ = bash(&"{a ".repeat(40_000));
        let _ = bash(&"{a; ".repeat(30_000));
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "{:?}", t.elapsed());
    }

    /// Eleventh review 8 Nov (zsh comparison, fifth run): a cd named by an expansion or run through eval, trap,
    /// emulate or functions[…]; CDPATH set any way; options through eval or a variable.
    #[test]
    fn eleventh_review_cases() {
        for c in [
            "c=cd; $c ~/Documents && rm -rf *", "$(echo cd) ~/Documents && rm -rf *", "set -- cd ~/Documents; \"$@\"; rm -rf *",
            "CMD='cd ~/Documents'; eval \"$CMD\"; rm -rf *", "eval \"$(cat go.sh)\" && rm -rf *", "c=pushd; ${c} ~/Documents; rm -rf *",
            "emulate zsh -c 'cd ~/Documents'; rm -rf *", "trap 'cd ~/Documents' DEBUG; rm -rf *", "functions[f]='cd ~/Documents'; f; rm -rf *",
            "set -A cdpath ~; cd Documents && rm -rf *", "x=CDPATH; typeset $x=~; cd Documents && rm -rf *", "IFS= read -r CDPATH <<< ~; cd Documents && rm -rf *",
            ": ${(A)cdpath::=~}; cd Documents && rm -rf *", "eval 'setopt chaselinks'; cd docs/.. && rm -rf *", "opt=chaselinks; set -o $opt; cd docs/.. && rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "eval \"$(direnv export zsh)\"; cargo build", "\"$VENV/bin/python\" x.py && rm -rf build", "$PY -m pytest && rm -rf build",
            "trap 'rm -rf \"$tmp\"' EXIT; rm -rf build", "set -euxo pipefail; cd x && rm -rf build", "grep -n CDPATH ~/.zshrc; cd crates && rm -rf target",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
    }

    /// Twelfth review 8 Nov (zsh comparison, sixth run): the unknown folder is no real path, so `tmp/…` and `..`
    /// from it stay unknown; trap, eval and programs by expansion beside a cd refuse only what may change folder.
    #[test]
    fn twelfth_review_cases() {
        let docs = |c: &str| bash_in(c, "/Users/t/Documents");
        for c in [
            "cd \"$X\" && rm -rf tmp/build", "cd \"$X\" && cd tmp && rm -rf build", "pushd \"$X\" && cd tmp && rm -rf build",
            "cd \"$X\" && cd private/tmp && rm -rf build", "cd \"$X\" && cd tmp/x && rm -rf *", "cd \"$X\" && cd ../../tmp && rm -rf build",
            "cd \"$(git rev-parse --show-toplevel)\" && cd tmp && rm -rf build", "source .venv/bin/activate && cd tmp && rm -rf build",
            "cd ~/Vault && cd - && cd tmp && rm -rf build", "eval \"cd ~\" && cd tmp && rm -rf build",
            "`true` cd tmp && rm -rf build", "$(true) cd tmp && rm -rf build", "${DRY:+echo} cd tmp && rm -rf build",
        ] {
            assert!(docs(c).is_some(), "{c:?}");
        }
        assert_eq!(docs("cd \"$X\" && cd /tmp/x && rm -rf build"), None, "an absolute cd makes the folder known again");
        for c in [
            "tmp=$(mktemp -d); trap 'rm -rf \"$tmp\"' EXIT; cd x && rm -rf build", "trap 'echo done' EXIT; cd x && rm -rf build",
            "eval \"$(direnv export zsh)\"; rm -rf build", "eval \"$(ssh-agent -s)\" && rm -rf build", "eval \"$(brew shellenv)\"; rm -rf build",
            "eval \"$(pyenv init -)\"; rm -rf build", "cd x && $PYTHON -m pytest; rm -rf build", "cd x && ${CC:-cc} main.c && rm -rf build",
            "cd x && \"$EDITOR\" f; rm -rf build", "unset CDPATH; cd x && rm -rf build", "export CDPATH=; cd x && rm -rf build",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        for c in ["eval \"$(zoxide init zsh)\"; rm -rf build", "eval \"$(cat go.sh)\" && rm -rf build", "c=cd; $c ~/Documents && rm -rf *"] {
            assert!(bash(c).is_some(), "{c:?}");
        }
    }

    /// Thirteenth review 8 Nov: from the unknown folder, relative writes into protected folders and Keychain reads are
    /// still refused; an environment tool's eval passes only alone.
    #[test]
    fn thirteenth_review_cases() {
        for c in [
            "cd \"$H\" && cp a.plist Library/LaunchAgents/x.plist", "cd \"$H\" && echo x > Library/LaunchAgents/x.plist",
            "cd \"$H\" && touch Library/LaunchAgents/x.plist", "cd \"$H\" && ln -s /x Library/LaunchAgents/x.plist",
            "cd - && cp x.plist Library/LaunchAgents/", "cd \"$H\" && sed -i '' s/a/b/ Library/Preferences/com.apple.dock.plist",
            "cd \"$H\" && plutil -replace a -bool true Library/Preferences/com.apple.dock.plist", "cd \"$H\" && cp x ../Library/LaunchAgents/x.plist",
            "cd \"$H\" && cat Library/Keychains/login.keychain-db", "eval \"$(brew shellenv; echo cd ~/Documents)\"; rm -rf *",
            "eval \"$(fnm env && cd ~)\"; rm -rf *",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in ["cd \"$H\" && cp a.txt notes/b.txt", "cd \"$H\" && cat README.md", "eval \"$(/opt/homebrew/bin/brew shellenv)\"; rm -rf build"] {
            assert_eq!(bash(c), None, "{c:?}");
        }
    }

    /// Fourteenth review 8 Nov: protected paths in any letter case, a protected folder in a variable the line assigned,
    /// and a project's own etc/, System/ or ../backup after an unknown cd left alone; direnv's 2>/dev/null passes.
    #[test]
    fn fourteenth_review_cases() {
        for c in [
            "cp a.plist ~/library/launchagents/x.plist", "cat ~/library/keychains/login.keychain-db", "cp x /ETC/hosts",
            "cp a.plist /library/launchdaemons/x.plist", "echo x > ~/library/preferences/com.apple.dock.plist",
            "cd ~/library && cp a.plist LaunchAgents/x.plist", "LA=~/Library/LaunchAgents; cp a.plist \"$LA/x.plist\"",
            "LA=$HOME/Library/LaunchAgents; cd \"$LA\" && cp a.plist x.plist", "D=~/Documents; rm -rf \"$D/x\"",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "eval \"$(direnv export zsh 2>/dev/null)\"; rm -rf build", "cd \"$WT\" && cp a.json etc/a.json", "cd \"$WT\" && mkdir -p System/x",
            "cd \"$WT\" && echo x > etc/conf", "cd \"$WT\" && cp -r src ../backup", "cd \"$WT\" && cp .env.example .env",
            "B=/Users/t/Work/Projects/X/build; rm -rf \"$B\"", "X=a; X=b; echo $X",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
    }

    /// Fifteenth review 8 Nov: a line's variables hold every value it gives them (`+=`, reassignment, chains, `for`),
    /// one set in a way the gate cannot follow refuses the delete, writes are judged through links, and a relative
    /// assigned value is a known folder for `cd`.
    #[test]
    fn fifteenth_review_cases() {
        for c in [
            "D=~/Work/Projects/P; D+=/../..; rm -rf \"$D\"", "D=~/Work/Projects/P/build; D+=/../../../../Documents; rm -rf \"$D\"",
            "LA=~/Library; LA+=/LaunchAgents; cp a.plist \"$LA/x.plist\"",
            "D=build; for D in ~/Documents; do rm -rf \"$D\"; done", "ROOT=~/Documents; OUT=$ROOT/out; rm -rf \"$OUT\"",
            "ROOT=~/Library/LaunchAgents; F=$ROOT/x.plist; cp a.plist \"$F\"", "D=/tmp/x; D=~/Documents; rm -rf \"$D\"",
            "for D in ~/Documents; do rm -rf \"$D\"; done", "cp a.plist \"/Volumes/Macintosh HD/Users/t/Library/LaunchAgents/x.plist\"",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "D=build; cd \"$D\" && rm -rf out", "D=x; cd \"$D\" && make && cd .. && rm -rf build", "D=build; rm -rf \"$D\"",
            "ROOT=/Users/t/Work/Projects/P; OUT=$ROOT/build; rm -rf \"$OUT\"", "OUT=build; OUT+=/release; rm -rf \"$OUT\"",
            "for f in *.tmp; do rm -f \"$f\"; done", "for f in $(ls); do rm -f \"$f\"; done", "D=/tmp/work; mkdir -p \"$D\" && cd \"$D\" && rm -rf *",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        // A project link to the user's launch agents: a write through it is a write there.
        let t = tempfile::Builder::new().prefix(".la-link-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let project = user.join("Work/Projects/H");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(user.join("Library/LaunchAgents")).unwrap();
        std::os::unix::fs::symlink(user.join("Library/LaunchAgents"), project.join("la")).unwrap();
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &user.join("Work"), &user, Some(&project));
        assert!(run("cp a.plist la/x.plist").is_some());
        assert!(run("echo x > la/x.plist").is_some());
        assert_eq!(run("cp a.plist notes/x.plist"), None);
    }

    /// Sixteenth review 8 Nov: assignments after keywords, arrays and subscripts, `${D:?}`, moves through a variable,
    /// value length bounded, `$(mktemp …)` as a temporary folder.
    #[test]
    fn sixteenth_review_cases() {
        for c in [
            "if [ -n \"$CI\" ]; then D=build; else D=~/Documents; fi; rm -rf \"$D\"", "dirs=(build ~/Documents); rm -rf $dirs",
            "dirs=(build ~/Documents); rm -rf \"$dirs[1]\"", "dirs=(build ~/Documents); rm -rf \"${dirs[@]}\"", "D=~/Documents; mv \"$D\" /tmp/",
            "D=~/Documents; rm -rf \"${D:?}\"/*",
        ] {
            assert!(bash(c).is_some(), "{c:?}");
        }
        for c in [
            "tmp=$(mktemp -d); rm -rf \"$tmp\"", "T=$(mktemp); echo x > \"$T\"; rm -f \"$T\"", "TMPD=$(mktemp -d -t x); rm -rf \"$TMPD\"",
            "tmp=$(mktemp -d) && cd \"$tmp\" && touch a && rm -rf \"$tmp\"", "D=build; rm -rf \"${D:?}\"", "D=build; rm -rf \"${D:?}\"/*",
            "dirs=(build dist); rm -rf \"${dirs[@]}\"", "D=build; mv \"$D\" /tmp/",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        // Accepted limit (correctness review 8 Nov): a variable the gate cannot follow (`read`, `print -v`, `${D::=…}`, a
        // substitution other than mktemp's) falls to the rule for unknowable words, as one from outside the line does,
        // so ordinary `ROOT=$(git rev-parse --show-toplevel); rm -rf "$ROOT/target"` passes.
        for c in [
            "ROOT=$(pwd); rm -rf \"${ROOT:?}/build\"", "ROOT=$(git rev-parse --show-toplevel); rm -rf \"$ROOT/target\"",
            "SCRIPT_DIR=\"$(cd \"$(dirname \"$0\")\" && pwd)\"; rm -rf \"$SCRIPT_DIR/build\"",
            "find . -name '*.orig' | while read -r f; do rm -f \"$f\"; done", "while IFS= read -r f; do rm -f \"$f\"; done < stale.txt",
            "for f in *.png; do rm -f \"${f%.png}.jpg\"; done", "OUT=\"${OUT:-build}\"; rm -rf \"$OUT\"", "OUT=build; rm -rf \"${OUT:-build}\"",
            "tmp=\"$(mktemp -d \"${TMPDIR:-/tmp}/review.XXXXXX\")\"; rm -rf \"$tmp\"", "tmp=$(mktemp -d 2>/dev/null || mktemp -d -t x); rm -rf \"$tmp\"",
            "x=`mktemp -d`; rm -rf \"$x\"", "dirs=(a b); dirs+=(c); rm -rf \"${dirs[@]}\"",
        ] {
            assert_eq!(bash(c), None, "{c:?}");
        }
        assert!(bash("OUT=~/Documents; rm -rf \"${OUT:-build}\"").is_some(), "the value and the default are both judged");
        assert!(bash("dirs=(a b); dirs+=(~/Documents); rm -rf \"${dirs[@]}\"").is_some(), "an added element is judged");
        // A value naming its own variable is judged once, never expanded forever.
        let _ = bash("D='$D'; rm -rf \"$D\"");
        let doubling: String = (1..40).map(|n| format!("A{n}=$A{}$A{}; ", n - 1, n - 1)).collect();
        let t = std::time::Instant::now();
        let _ = bash(&format!("A0=xxxxxxxx; {doubling}rm -rf \"$A39\""));
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "{:?}", t.elapsed());
    }

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

    /// Bug notes 2026-11-08T13-03-44 and 2026-11-08T12-27-04 (~13:05: "Allow it"): the integrating install
    /// may click exactly Heimdall's Quit menu item ("Quit Heimdall", "Quit <name>" after a rename) while its new build
    /// is staged, in the forms docs/personal/nightly-build.md section 4 and approvals.log (8 Nov 11:50) use. Nothing broader:
    /// other clicks on Heimdall, the same click with no staged build, and every other way to quit it are refused, and
    /// a here-document script is now read (the installed gate let any press through one).
    #[test]
    fn an_install_may_click_exactly_heimdalls_quit_item_and_nothing_broader() {
        let t = tempfile::Builder::new().prefix(".quit-install-test-").tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let user = t.path().join("home");
        let vault = user.join("Work");
        std::fs::create_dir_all(&vault).unwrap();
        let run = |c: &str| refuse("Bash", &json!({"command": c}), &vault, &user, Some(&vault));
        let doc = "osascript <<'EOF'\ntell application \"System Events\" to tell process \"Heimdall\"\n  click menu bar item 1 of menu bar 1\n  delay 0.4\n  click (first menu item of menu 1 of menu bar item 1 of menu bar 1 whose name begins with \"Quit \")\nend tell\nEOF\nfor i in $(seq 1 60); do pgrep -x Heimdall >/dev/null || break; sleep 1; done";
        let allowed = [
            doc.to_owned(),
            // approvals.log, 8 Nov 11:50 (refused then).
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\"' -e 'click menu bar item 1 of menu bar 1' -e 'delay 0.4' -e 'click (first menu item of menu 1 of menu bar item 1 of menu bar 1 whose name begins with \"Quit \")' -e 'end tell' 2>&1; pgrep -x Heimdall".into(),
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to click menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 1'".into(),
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to click menu item \"Quit Ivy\" of menu 1 of menu bar item 2 of menu bar 2'".into(),
            "osascript -e 'tell application \"System Events\"' -e 'tell process \"Heimdall\"' -e 'click menu bar item 1 of menu bar 2' -e 'click menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 2' -e 'end tell' -e 'end tell'".into(),
        ];
        let neighbours = [
            // Other items of Heimdall's menu, its cards and windows.
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to click menu item \"Settings…\" of menu 1 of menu bar item 1 of menu bar 1'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to click menu bar item 1 of menu bar 1'",
            "osascript -e 'tell application \"System Events\" to click button \"Allow\" of window 1 of process \"Heimdall\"'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to click button \"Allow\" of window 1'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to perform action \"AXPress\" of menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 1'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to click (first menu item of menu 1 of menu bar item 1 of menu bar 1 whose name begins with \"Q\")'",
            // The quit click with more after or before it.
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\"' -e 'click menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 1' -e 'click button 1 of window 1' -e 'end tell'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\"' -e 'keystroke \"1\"' -e 'click menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 1' -e 'end tell'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\"' -e 'click menu bar item 1 of menu bar 1' -e 'click menu bar item 1 of menu bar 1' -e 'click menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 1' -e 'end tell'",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\"' -e 'delay 600' -e 'click menu item \"Quit Heimdall\" of menu 1 of menu bar item 1 of menu bar 1' -e 'end tell'",
            // Presses through a here-document.
            "osascript <<'EOF'\ntell application \"System Events\" to click button \"Allow\" of window 1 of process \"Heimdall\"\nEOF",
            "osascript <<EOF\ntell application \"System Events\" to keystroke \"1\"\nEOF",
            "osascript - <<'EOF'\ntell application \"System Events\" to click at {100, 200}\nEOF",
            // Quitting it any other way skips its drain.
            "osascript -e 'quit app \"Heimdall\"'", "osascript -e 'tell application \"Heimdall\" to quit'",
            "osascript <<'EOF'\ntell application \"Heimdall\"\nquit\nend tell\nEOF",
            "osascript -l JavaScript -e \"Application('Heimdall').quit()\"",
            "osascript -e 'tell application \"System Events\" to tell process \"Heimdall\" to quit'",
            "killall Heimdall", "pkill -x Heimdall", "pkill -f /Users/t/Applications/Heimdall.app/Contents/MacOS/Heimdall",
        ];
        for c in &neighbours { assert!(run(c).is_some_and(|m| m.contains("deny list")), "{c}"); }
        // With no build staged even the exact click is refused, saying who may quit and when.
        for c in &allowed { assert!(run(c).is_some_and(|m| m.contains("GO INSTALL")), "{c}"); }
        let stage = user.join("Applications/.Heimdall.app.stage/Contents");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("Info.plist"), "x").unwrap();
        // Review 1110 item 3: a staged build alone is not enough (any agent can make the file); the click also needs
        // Heimdall's GO INSTALL grant (`grants::write_quit`).
        for c in &allowed { assert!(run(c).is_some_and(|m| m.contains("GO INSTALL") && m.contains("15 minutes")), "{c}"); }
        let go = |s: &Scope| *s == Scope::QuitInstall;
        let run_go = |c: &str| refuse_with("Bash", &json!({"command": c}), &vault, &user, Some(&vault), &go);
        for c in &allowed { assert_eq!(run_go(c), None, "{c}"); }
        for c in &neighbours { assert!(run_go(c).is_some_and(|m| m.contains("deny list")), "{c}"); }
        std::fs::remove_file(stage.join("Info.plist")).unwrap();
        for c in &allowed { assert!(run_go(c).is_some_and(|m| m.contains("new build staged")), "a grant without a stage: {c}"); }
        // Ordinary System Events work and lab helpers pass.
        for c in ["osascript -e 'tell application \"System Events\" to get name of every process'", "pkill -f heimdall-lab-drama", "killall Arsenal",
            "osascript <<'EOF'\ntell application \"System Events\" to get name of every process\nEOF"] {
            assert_eq!(run(c), None, "{c}");
        }
    }

    /// Bug note 2026-11-08T02-03-44: a job releases the memory need it declared without the user's click, and only that:
    /// one `memory-needs/<name>.json` by its absolute path, never the folder, a glob, a recursive delete, another file
    /// in the data folder, a link out of it, or the same path when the hook does not know the data folder.
    #[test]
    fn a_job_may_delete_its_own_memory_need_file_and_nothing_beside_it() {
        // The user's data folder is outside every allowed area (a temporary folder would be one already).
        let data = user().join("Library/Application Support/Heimdall");
        let folder = data.join("memory-needs").display().to_string();
        let need = format!("{folder}/angel-nemotron-wer.json");
        let r = |c: &str| refuse_in(Some(&data), "Bash", &json!({"command": c}), &vault(), &user(), Some(Path::new("/Users/t/Work")), &|_| false);
        // The night's two commands (01:39:55, 01:42:41), and the plain forms.
        for ok in [format!("rm -f \"{need}\""), format!("rm \"{need}\""), format!("unlink \"{need}\""), format!("rm -fv \"{need}\""),
            format!("vella unload nemotron-3.5-transcribe 2>&1 | tail -1; rm -f \"{need}\"; ls \"{folder}/\""),
            format!("rm -f \"{need}\"; ls \"{folder}/\" 2>&1; sleep 5; pgrep -f \"Heimdall Drama\" >/dev/null && echo running")] {
            assert_eq!(r(&ok), None, "{ok}");
        }
        for bad in [format!("rm -rf \"{folder}\""), format!("rm -rf \"{folder}/\""), format!("rm -f \"{folder}\"/*.json"), format!("rm -rf \"{need}\""),
            format!("rm -r \"{need}\""), format!("rm -d \"{need}\""), format!("rmdir \"{folder}/dir.json\""),
            format!("rm -f \"{folder}/.hidden.json\""), format!("rm -f \"{folder}/.json\""), format!("rm -f \"{folder}/x.txt\""),
            format!("rm -f \"{}\"", data.join("settings.json").display()), format!("rm -f \"{folder}/../settings.json\""),
            format!("rm -f \"{folder}/sub/x.json\""), format!("trash \"{need}\""), format!("N=x; rm -f \"{folder}/$N.json\""),
            format!("rm -f ~/Library/Application\\ Support/Heimdall/memory-needs/x.json"),
            format!("find \"{folder}\" -name '*.json' -delete"), format!("rsync -a --delete /tmp/e/ \"{folder}/\"")] {
            assert!(r(&bad).is_some(), "{bad}");
        }
        assert!(refuse("Bash", &json!({"command": format!("rm -f \"{need}\"")}), &vault(), &user(), None).is_some(), "no data folder, no release");
        // On disk: a folder named like a need, and a `memory-needs` that is a link elsewhere, are no need files.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let real = root.join("data");
        std::fs::create_dir_all(real.join("memory-needs/dir.json")).unwrap();
        std::fs::write(real.join("memory-needs/job.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/x.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("lab")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("lab/memory-needs")).unwrap();
        let none = |_: &Scope| false;
        let ctx = |d: &Path| Ctx { vault: Path::new("/v"), user: Path::new("/u"), granted: &none, needs: vec![d.join(NEEDS)],
            moved: Default::default(), keychain_used: Default::default(), other_security: Default::default(), skill_line: Default::default(), clock: Default::default(), roots_cache: Default::default(), cdpath: Default::default(), plain: Default::default(), home_set: Default::default(), cd_token: Default::default(), cd_vars: Default::default(), set_cd: Default::default(), vars: Default::default(), expanding: Default::default() };
        let w = |p: &Path| vec!["rm".to_string(), "-f".into(), p.display().to_string()];
        let check = |d: &Path, p: PathBuf| ctx(d).memory_need("rm", &w(&p), &p.display().to_string(), &p);
        assert!(check(&real, real.join("memory-needs/job.json")), "a need file");
        assert!(!check(&real, real.join("memory-needs/dir.json")), "a folder named like one");
        assert!(!check(&root.join("lab"), root.join("lab/memory-needs/x.json")), "through a link");
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
