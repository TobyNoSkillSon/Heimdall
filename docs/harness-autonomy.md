# Harness autonomy: no agent stops to ask

When a harness is set up, its agents get the most autonomous mode it has, so they never ask anything. Nobody watches
the threads Heimdall starts, so a prompt is a stall: a delegated child once waited 22 minutes on `cat >> file <<EOF`.

So every harness on the Mac runs in its most autonomous mode, set by configuration, and Heimdall checks it at each
start. One short deny list is all that is refused, and a refusal is an immediate answer the agent reads, never a wait.

## The deny list

Written once, in `crates/emergency-cli/src/deny.rs`. Everything not on it is approved.

1. **Deleting outside the workspace and scratch.** `rm`, `rmdir`, `unlink`, `srm`, `trash`, `find -delete`, `find -exec`
   (or `-execdir`, `-ok`) with one of those or a move into scratch, the same fed through `| xargs`, Finder's `delete`
   through `osascript`, and `rsync --delete` whose target is outside the home folder chosen in Setup (two folders deep, so `<home>/Projects/X` but never `<home>/Projects`),
   the temporary folders (`/tmp`, `/private/tmp`, `/var/folders`, `$TMPDIR`) or the build caches (Xcode DerivedData,
   `~/Library/Caches`, `~/.cache`, `~/.t3/worktrees`, `~/.t3/scratch`). A target that would become the root if a
   variable were empty (`"$D"/*`) is refused with how to guard it (`"${D:?}"`). A `mv` of something outside those
   areas into a temporary folder or `~/.Trash` counts as a delete, and so does deleting, later in the same command
   line, what such a `mv` moved. Moving it into the home folder alone passes.
2. **Disk formatting.** `diskutil` erase, partition and zero verbs, `diskutil apfs deleteVolume`, `newfs*`, `fdisk`,
   `gpt`, `asr`, and `dd` or a redirect onto `/dev/disk*`.
3. **The Keychain.** Any `security` verb but the read-only certificate ones (never `list-keychains -s`, nor `-i` or
   `-p` before the verb, which run commands from input), and any tool on `~/Library/Keychains`.
4. **System settings.** `sudo`, `systemsetup`, `csrutil`, `nvram`, `fdesetup`, `spctl`, `tccutil`, `pmset` writes,
   `scutil --set`, `networksetup` changes, `defaults write` to Apple or global domains, `launchctl` changes, software
   updates, profiles, login-item changes, and writes into `LaunchAgents`, `LaunchDaemons`, `/etc`, `/System`,
   `/Library/Preferences` or `~/Library/Preferences/com.apple.*`. Reading the login items needs no yes
   (`osascript` with System Events `get … of every login item`, `launchctl list`, `ls ~/Library/LaunchAgents`);
   `sfltool dumpbtm` stays refused because it asks for an admin password on the user's screen. So are System Events
   keystrokes, a `click at` a point, presses aimed at Heimdall and `cliclick`: they act on the user's screen and Heimdall's cards.

Deleting model weights under `~/.cache/huggingface` is refused too (they need the user's yes). A variable in a delete
path (`~/$X`, `/tmp/$X`) is judged by the folder before it, so an unguarded one outside the allowed areas is refused
with how to guard it.

The command reader follows the shell's quoting, ANSI-C `$'…'` strings included and decoded as the shell does (a 5 Nov
review hid a `security dump-keychain` between two `$'\''`; a second spelled `/` and `rm` in escapes). A word spelled
with them is never a grantable literal. It is a guard against an agent's mistake, not a security boundary: a Python script can still delete anything, a `mv`
into the home folder followed by an `rm` in a later command passes, and an agent could still edit the harness settings that
install the hook.
It guards Claude Code only: Codex, OpenCode and Grok run allow-all with no
Heimdall hook, so for them the deny list is not enforced (Heimdall starts only Claude agents for the user's work).

Claude's own safety checks ("Dangerous rm operation on possibly-empty variable path", bug note 2026-11-04T17-57-58)
reach the `PermissionRequest` hook too: on 4 Nov 17:55:23 it allowed that task's in-workspace `rm -f $S/…` at once (the
transcript records source hook, `safetyCheck`, `dangerousRemoval`) and the task ran on. T3's "Waiting for approval"
card was a request T3 raised beside the hook's answer and never cleared; that is T3's state, not a wait.

## What each harness gets

`heimdall-emergency harness check` reports drift; `harness apply` writes it back, keeps every other key, and backs each
file up under `<data>/harness-backups` before its first rewrite (`crates/emergency-cli/src/harness.rs`).

| Harness | Setting | Why |
|---|---|---|
| Claude Code, every config folder (`~/.claude`, `~/.claude-*`, each T3 instance's `homePath`) | `permissions.defaultMode = "bypassPermissions"`, `skipDangerousModePermissionPrompt = true`, no `ask` rules, no `disableBypassPermissionsMode` or `block*OutsideWorkingDirectories` | The most autonomous mode. Without the skip key a background session ignores the bypass request. |
| | deny rules: `AskUserQuestion` plus plain forms of the deny list (`Bash(sudo *)`, `Bash(diskutil erase*)`, Keychain reads, `launchctl` changes) | Deny rules hold in every mode. No `rm` rule: a Bash rule is a prefix match, so `Bash(rm -rf /)` refused every `rm -rf /Users/…`. Path rules live in the hook. |
| | `PreToolUse` hook `heimdall-emergency hook pretool` | Refuses the deny list before any tool runs, in every mode. |
| | `PermissionRequest` hook `heimdall-emergency hook permission` | Answers any prompt that is still raised: allow, unless it is on the deny list. Every session, delegated children at any depth included. A question is left alone; a `[heimdall-bg]` background call is declined so it stops. |
| T3 Code (`~/.t3/userdata/settings.json`) | each Claude instance's `environment`: `CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1` | The only switch for the checks `bypassPermissions` keeps (a critical-path `rm`, "Dangerous rm operation not ruled out", which heredocs and chained commands trip). Claude reads it only from its launch environment, never from settings. T3 reloads the file itself; new sessions get it at once. |
| Codex (`~/.codex/config.toml`) | `approval_policy = "never"`, `sandbox_mode = "danger-full-access"`; `rules/default.rules`: `prefix_rule(pattern=["rm"], decision="allow")` | Under `never`, Codex forbids a forced `rm` outright unless a rule allows it. |
| OpenCode (`~/.config/opencode/opencode.json[c]`) | `"permission": {"*": "allow", "question": "deny"}` | `allow` covers `doom_loop`, `external_directory` and `.env` reads; the question tool is the one thing left that asks. |
| Grok (`~/.grok/config.toml`) | `[ui] permission_mode = "always-approve"`, `yolo = true`; `[features] ask_user_question = false` | Grok's documented maximum, plus no question card. |

T3's own `full-access` runtime mode is its default and maps to each driver's maximum (Claude `bypassPermissions`,
Codex `never` + full access, OpenCode allow-all, Grok `--always-approve`). Heimdall's background calls keep
`approval-required` on purpose: they use no tools.

## Why prompts appeared under full access

- Claude's `bypassPermissions` still asks for a critical-path `rm` and for any command its parser cannot trace
  ("Dangerous rm operation not ruled out"): heredocs, `cd` then `rm`, `$VAR/*`, `bash -c`. Under T3 that goes to the
  SDK's permission callback, which T3 shows as a card nobody can click.
- Heimdall's old `PermissionRequest` hook answered only Heimdall's recorded threads, approved only scratch deletes and
  declined everything else; a nested delegate child was not recognised and got no
  answer at all.

## When it runs

- **Every Heimdall start** (`crates/app/src/harness_autonomy.rs`, on its own thread). If anything was set back, one
  island line: "No-prompt mode restored · Claude Code, Codex". Nothing drifted: nothing shown.
- **Setup**: when the setup wizard finishes and when T3 is connected, so a fresh machine ends in this mode.
- **By hand** after a harness update: `"$HOME/Applications/Heimdall.app/Contents/MacOS/heimdall-emergency" harness apply`.

Every path runs the same `harness_autonomy::pass` (`harness::ensure` with the installed CLI's hooks and backups under
`<data>/harness-backups`). Only an installed app writes (an app bundle in `~/Applications` or `/Applications` with its CLI beside it); candidates in `dist/`, lab and test builds
leave the real settings alone.

## First run for a new user

- **Setup** has a Coding Agents step after T3 Code. It lists each harness it found on one line (its
  mark, its name, "Asks first" or "Never asks") and the deny list as four short lines ("No deletes outside your home
  folder", "No disk erasing", "No Keychain access", "No system settings", from `deny::SUMMARY`). An orange shield marks
  each deny line and each harness it guards (Claude Code and T3 Code, `harness::GUARDED`); with only Codex, OpenCode or
  Grok found the deny lines are not shown, since nothing holds those to it. A config the check cannot read (a `.jsonc`
  with comments) shows "Asks first". Set Up is the default button; Leave as Is is beside it, and after Leave as Is a
  reopened Setup makes Leave as Is the default. The choice is saved as `harness_autonomy` in settings.json: Set Up
  applies at once (no island line) and at every start after; Leave as Is never touches a harness. Before the step
  (T3's Connect comes first) nothing is touched. With every harness already set, or none found, the step shows plain
  Continue. Renders: `heimdall-ui-demo --wizard <dir>` (`5-agents*.png`).
- **The first-run prompt** (`setup_wizard::greeting`, sent once at Setup's Done): when the harnesses were set, it tells
  the agent to run `heimdall-emergency --data-dir <data> harness check` once with the installed CLI's path and tell the
  user in one sentence which agents are set, or what is not and why; it also says the deny list guards Claude agents
  only. After Leave as Is it says nothing about them. The public   `onboarding` skill repeats the rule.
- Tests: `crates/app/src/harness_autonomy_tests.rs` (a fresh profile on an isolated home with fake Claude Code, T3,
  Codex, OpenCode and Grok configs: the step's rows, Leave as Is and no choice touch no byte, Set Up sets all five and
  backs each file up, the next start finds nothing, an update's drift is restored, a lab build never writes),
  `first_run_greeting_tests.rs` (the choice is saved through the real pipeline), and the wizard render's checks.

## Checked

Checked with T3 0.1.6 and Claude Code 2.1.314: a full-access thread on `claudeAgent` and its `delegate_task` child ran a
heredoc append, `du && rm -rf && ls`, a delete of the folder it had `cd`ed into, `rm -rf "$D"/*`, `pkill -f` on a dummy
process, and writes outside the working folder with Bash and the Write tool. All ran; T3 recorded no approval request.
Not checked live: the second Claude instance (its account was at its weekly limit), Codex, OpenCode and Grok (by
config only).
