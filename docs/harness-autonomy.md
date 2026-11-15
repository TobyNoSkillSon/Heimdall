# Harness autonomy: coding agents that never stop to ask

Heimdall can set every coding-agent harness on the Mac (Claude Code, T3 Code, Codex, OpenCode, Grok) to its most
autonomous mode and keep it there. Nobody watches the threads Heimdall starts, so a permission prompt is a stall. One
short deny list is the only thing refused, and a refusal is an answer the agent reads at once, never a wait.

Read this when: you decide on Setup's "Let coding agents work without asking?" step, an agent's command was refused
by Heimdall's deny list, a harness started asking again, you run `heimdall-emergency harness`, or you change the deny
list or a harness's settings.
Code: `crates/emergency-cli/src/harness.rs` (what each harness gets), `crates/emergency-cli/src/deny.rs` (the deny
list), `crates/emergency-cli/src/approvals.rs` (the two Claude hooks), `crates/app/src/harness_autonomy.rs` (when the
app applies it).

## Turn it on or off

The public edition asks once, in Setup's step 10, "Let coding agents work without asking?" ([setup.md](setup.md)).
The personal edition has no step and always applies it.

- The step lists each harness found on one line: its mark, its name, and "Asks first" or "Never asks". A config the
  check cannot read (a `.jsonc` with comments) shows "Asks first".
- Under them, the deny list as four lines (`deny::SUMMARY`): "No deletes outside your home folder", "No disk erasing",
  "No Keychain access", "No system settings". An orange shield marks each deny line and each harness the list guards
  (`harness::GUARDED`: Claude Code and T3 Code). With only Codex, OpenCode or Grok found, the deny lines are hidden,
  since nothing holds those to it.
- **Set Up** (the default button) applies at once and again at every start. **Leave as Is** never touches a harness.
  After Leave as Is, a reopened Setup makes Leave as Is the default.
- With every harness already set, or none found, the step shows plain Continue. Nothing is touched before the step.
- The choice is saved as `harness_autonomy` in `settings.json` ([settings.md](settings.md)). From a coding agent:
  `heimdall setup --harnesses set` or `--harnesses leave`.

Check it worked:

```sh
"/Applications/Heimdall Alpha.app/Contents/MacOS/heimdall-emergency" harness check
```

It prints `All set: Claude Code, …` and exits 0, or one `drift <harness>: <what> (<file>)` line per setting that
differs and exits 1.

## When it runs

`harness_autonomy::start` runs the same pass (`harness::ensure` with Heimdall's hooks) on its own thread:

- at every Heimdall start; anything set back shows one island line, "No-prompt mode restored · Claude Code, Codex";
  nothing drifted, nothing shown;
- when Setup finishes and when T3 Code is connected;
- at once when Set Up is chosen (no island line).

Only an installed app writes: an app bundle in `/Applications` or `$HOME/Applications` with `heimdall-emergency`
beside its binary (`harness_autonomy::installed_bundle`). Builds in `dist/`, lab and test builds leave the real
settings alone, so the hooks always point at the installed CLI.

After Set Up, the agent's first message after Setup (`setup_wizard::greeting`) tells it to run `harness check` once
with the installed CLI and tell the user in one sentence which agents are set, and that the deny list guards Claude
agents only.

## Apply it by hand

1. After a harness update reset its settings, run:

   ```sh
   "/Applications/Heimdall Alpha.app/Contents/MacOS/heimdall-emergency" harness apply
   ```

2. It writes back only the settings below, keeps every other key, and copies each file to
   `<data>/harness-backups/<harness>-<name>.<unix time>` before its first rewrite.
3. Check it worked: `harness check` prints `All set`.

Both verbs take `--data-dir <folder>`; the default is Heimdall's data folder ([home.md](home.md)). Exit codes: 0 done,
1 drift found (`check`) or a file it could not read or write, 2 misuse.

## What each harness gets

A harness that is not installed is skipped.

| Harness | Settings | Why |
|---|---|---|
| Claude Code, every config folder (`$HOME/.claude`, each `$HOME/.claude-*` holding a config, each T3 Claude instance's `homePath`) | `permissions.defaultMode = "bypassPermissions"`, `skipDangerousModePermissionPrompt = true`; no `ask` rules, no `disableBypassPermissionsMode`, `disableAutoMode` or `block{Reads,Writes}OutsideWorkingDirectories`. A `settings.local.json` loses any other `defaultMode` and its `ask` rules | The most autonomous mode. Without the skip key a background session ignores the bypass request |
| | deny rules `harness::CLAUDE_DENY`: `AskUserQuestion` plus the plainest forms of the deny list (`Bash(sudo *)`, `Bash(diskutil erase*)`, Keychain reads, `launchctl` changes) | Deny rules hold in every mode. There is no `rm` rule: a Bash rule matches as a prefix, so `Bash(rm -rf /)` would refuse every `rm -rf` of any absolute path. Path rules live in the hook |
| | `PreToolUse` hook `heimdall-emergency hook pretool` | Refuses the deny list before any tool runs, in every mode |
| | `PermissionRequest` hook `heimdall-emergency hook permission` | Answers any prompt still raised: allow, unless the deny list refuses it. Every session, delegated children at any depth included. A question is left alone; a `[heimdall-bg]` background call is declined so it stops |
| T3 Code (`$HOME/.t3/userdata/settings.json`) | each Claude instance's launch environment gets `CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1`; the default `claudeAgent` instance gets an entry for it | The only switch for the checks `bypassPermissions` keeps (a critical-path `rm`, "Dangerous rm operation not ruled out"). Claude reads it from its launch environment only. T3 reloads the file itself |
| Codex (`$HOME/.codex/config.toml`) | `approval_policy = "never"`, `sandbox_mode = "danger-full-access"`; in `rules/default.rules`: `prefix_rule(pattern=["rm"], decision="allow")` | Under `never`, Codex forbids a forced `rm` outright unless a rule allows it |
| OpenCode (`$HOME/.config/opencode/opencode.json` or `.jsonc`) | `"permission": {"*": "allow", "question": "deny"}` | `allow` covers `doom_loop`, `external_directory` and `.env` reads; the question tool is the one thing left that asks |
| Grok (`$HOME/.grok/config.toml`) | `[ui] permission_mode = "always-approve"`, `yolo = true`; `[features] ask_user_question = false` | Grok's documented maximum, and no question card |

- A managed `/Library/Application Support/ClaudeCode/managed-settings.json` that turns bypass mode off is reported as
  an error; only an administrator can change it.
- T3's own `full-access` runtime mode is its default and maps to each driver's maximum. Heimdall's background calls
  use `approval-required` on purpose: they use no tools ([background-call.md](background-call.md)).
- Codex, OpenCode and Grok run allow-all with no Heimdall hook, so the deny list is not enforced for them.
- Codex, OpenCode and Grok are checked against their config files only (`harness_autonomy_tests.rs`); no live
  session of theirs is tested.

Why both hooks: Claude's `bypassPermissions` still asks for a critical-path `rm` and for any command its parser cannot
trace (here-documents, `cd` then `rm`, `$VAR/*`, `bash -c`). Under T3 that becomes a card nobody can click; the
`PermissionRequest` hook answers it first.

## The deny list

Everything not on it is approved. Each refusal names `docs/harness-autonomy.md` and says what to do instead. Every
answer the hooks give is logged in `<data>/logs/approvals.log`.

1. **Deleting outside the allowed areas.** `rm`, `rmdir`, `unlink`, `srm`, `trash`, `find -delete`, `find -exec`
   (`-execdir`, `-ok`) running one of those or a move into a temporary folder, the same fed through `| xargs`,
   Finder's `delete` through `osascript`, and `rsync --delete` are refused unless every target is inside:
   - Heimdall's home folder, the one chosen in Setup, at least two folders below it (`<home>/a/b`, never `<home>/a`);
   - a temporary folder, at least one folder below it: `/tmp`, `/private/tmp`, `/var/folders`, `$TMPDIR`;
   - a build cache, at least one folder below it: `$HOME/Library/Developer/Xcode/DerivedData`, `$HOME/Library/Caches`,
     `$HOME/.cache`, `$HOME/.t3/worktrees`, `$HOME/.t3/scratch`.

   Model weights under `$HOME/.cache/huggingface` are refused even so. A target is judged as written and with its
   folders' symlinks resolved; both must be inside.
2. **Disk formatting.** `diskutil` erase, partition, zero and APFS delete verbs, `newfs*`, `fdisk`, `gpt`, `asr`, and
   `dd` or a redirect onto `/dev/disk*` or `/dev/rdisk*`.
3. **The Keychain.** Every `security` verb but the read-only certificate ones (`find-identity`, `find-certificate`,
   `list-keychains` without `-s`, `show-keychain-info`, `verify-cert`, `cms`, `error`, `help`), `security -i` or `-p`
   (they run commands from input), and any tool on `$HOME/Library/Keychains` or `/Library/Keychains`.
4. **System settings.** `sudo`, `systemsetup`, `csrutil`, `nvram` writes, `fdesetup`, `spctl` changes, `tccutil`,
   `pmset` writes, `scutil --set`, `networksetup` changes, `defaults write`, `delete` or `import` on Apple or global
   domains, `launchctl` changes, `softwareupdate`, `profiles`, `sysadminctl`, `dscl` changes, `kextload`,
   `shutdown`, `reboot`, `halt`, login-item changes through `osascript` or `sfltool`, and writes into `LaunchAgents`,
   `LaunchDaemons`, `/etc`, `/System`, `/Library/Preferences` or `$HOME/Library/Preferences/com.apple.*`.
   - Reading the login items passes: `osascript -e 'tell application "System Events" to get the name of every login
     item'`, `launchctl list`, `ls $HOME/Library/LaunchAgents`. `sfltool dumpbtm` stays refused: it asks for an admin
     password on the user's screen.
   - System Events keystrokes, a `click at` a point, presses aimed at Heimdall and `cliclick` are refused: they act on
     the user's screen. An `osascript` here-document is read like its `-e` lines.

A `mv` of something outside the allowed areas into a temporary folder or `$HOME/.Trash` counts as a delete, and so
does deleting, later in the same line, what such a `mv` moved. Moving it into the home folder passes.

The personal edition adds a few narrow exceptions and click-granted deletes of its own (`grants.rs`;
`grants::ENABLED` is false in the public edition, and `heimdall-emergency grants status` prints `grants: off`).

## Write commands the gate can judge

The gate reads a command line as zsh would and follows the folder the shell is in. It is a guard against an agent's
mistake, not a security boundary: a Python script can still delete anything, a `mv` into the home folder followed by
an `rm` in a later command passes, and an agent could edit the harness settings that install the hook.

- **Name delete targets in full.** A `cd` may fail, so after one both folders are judged; `&&` and `||` run or skip
  what follows accordingly. From the home folder itself, `cd x; rm -rf build` is refused (a failed `cd` would delete
  `<home>/build`) and `cd x && rm -rf build` passes.
- **Expansions make the folder unknown.** A `cd` to an expansion other than `$HOME/…` goes to an unknown folder, so a
  relative delete after it is refused: write `rm -rf "$WT/target"`, not `cd "$WT" && rm -rf target`. `source`, `.`,
  zsh's short `if [[ … ]] cd …`, options that change `cd` (chaselinks, autopushd and kin) and a line that sets CDPATH
  or HOME do the same: `source .venv/bin/activate && rm -rf build` is refused; run `.venv/bin/python` directly or name
  the folder in full.
- **Guard variables.** A target that would become the root if a variable were empty (`"$D"/*`) is refused with how to
  guard it: `"${D:?}"`. A variable in a delete path is judged by the folder before it.
- **Substitutions.** `$(…)` and unquoted or double-quoted backticks are substitutions. A program written as an
  expansion that may be a remover (`$RM`, `$(which rm)`) or a shell's `-c` is judged as one.
- **Quoting.** ANSI-C `$'…'` strings are decoded as the shell does, here-documents are read to their exact tag, and
  brace expansion is expanded.
- **What the gate follows.** `cd -P` resolves links; `pushd` and `popd` pairs are followed; a subshell's or a
  background `cd` ends with it; an `exit` ends a path only on a plain line (no if, case, loop, function or `{ … }`
  group); from the first loop or function definition whose body changes folder, the folder is unknown.

Limits; past any of them the line is refused and the message says to split it or name folders in full:

| Limit | Value |
|---|---|
| Line length | 128 KB |
| Possible folder states | 128 |
| Brace expansion | 1,024 words, 64 brace groups in one word |
| Nested groups or substitutions | 64 |
| Time to judge | 3 s (`deny::DEADLINE`; the hook's own timeout is 10 s, and a timed-out hook lets the call run) |

## Tests

```sh
cargo test -p heimdall-emergency
cargo test -p app harness_autonomy
```

- `crates/emergency-cli/src/deny.rs`: the deny list and the gate's reading of shell lines.
- `crates/emergency-cli/src/approvals.rs`: both hooks' answers.
- `crates/emergency-cli/src/harness.rs`: each harness's settings and drift.
- `crates/app/src/harness_autonomy_tests.rs`: a fresh profile on an isolated home with fake Claude Code, T3, Codex,
  OpenCode and Grok configs. Leave as Is and no choice touch no byte; Set Up sets all five and backs each file up; the
  next start finds nothing; an update's drift is restored; a lab build never writes.
- `crates/app/src/pipeline/tests/first_run_greeting_tests.rs`: the choice saved through the real pipeline.
- `heimdall-ui-demo --wizard <dir>` (feature `lab`): renders the step (`09-agents*.png`) and checks its buttons.

## See also

- [setup.md](setup.md): the Setup step and `heimdall setup --harnesses`
- [t3-orchestration.md](t3-orchestration.md): T3 Code tasks and approvals
- [background-call.md](background-call.md): background calls, which keep `approval-required`
- [home.md](home.md): the home folder the deny list allows deletes in
- [resilience.md](resilience.md): logs and recovery
