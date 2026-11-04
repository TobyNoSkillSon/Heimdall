# Home: where Heimdall lives

The user picks one folder as Heimdall's home in the setup wizard. The agent (Pi) works in it, and every file of
Heimdall's own (settings, logs, archive, agent state, bug notes, drops, shots, clips) lives in `<home>/.heimdall`.
Both editions. The code is `crates/home` (`heimdall_home`); the app's side is `crates/app/src/home_change.rs`.

## The pointer

The only file outside home: `~/Library/Application Support/<bundle id>/home.json`, one per edition (personal
`dev.heimdall.app`, public `com.tobynoskillson.heimdall`), so the public app never reads the personal profile.

```json
{ "home": "/Users/ana/Notes" }
```

While a move waits it also has `data` (where the files still are) and `pending` (`from`, `to_home`, `leave_link`).

Resolution, first match wins: `HEIMDALL_DATA` (with `HEIMDALL_HOME`) from the environment; the pointer; in the
personal edition only, the profile from before home folders (`~/Library/Application Support/Heimdall`, with its old home folder
when it exists, else `~`), used in place until its owner moves it; otherwise a fresh profile in `~/.heimdall`.
`Location::needs_choice()` is true for the last two, which is when the wizard shows its home step.

## Changing home

1. `home_change::plan(new_home)`: checks the folder (absolute, exists, writable, not inside `.heimdall`, no
   non-empty `.heimdall` there yet) and says what will happen: size, one rename or a copy across volumes. Writes
   nothing.
2. `home_change::request(new_home)`: records the move in the pointer (or just the new home when nothing exists yet).
   Refused when the app was started on a named folder (`--data-dir`, `HEIMDALL_DATA`), so lab runs never touch it.
3. `home_change::relaunch()`, then the caller quits: a helper waits for this process to end and runs
   `open -n <the app>`.
4. At the next start, before anything opens the data folder, `main` calls `heimdall_home::apply_pending`: one rename on
   the same volume; otherwise copy to `.heimdall.partial`, compare entry counts and bytes, rename, point the pointer
   at it, then remove the old folder. A failure drops the pending move and leaves the files where they were; the log
   says `Heimdall home: …` either way. The personal edition's old profile keeps a link at its old path, since other
   tools still name it.

## Environment

The app sets these at start, so every child (Pi and the CLIs it runs, workers, the emergency CLI, islands) uses the
same folders. Pi's environment is cleared and these are passed through by name (`crates/agent/src/harness.rs`).

- `HEIMDALL_DATA`: the data folder. `--data-dir` sets it for that run.
- `HEIMDALL_HOME`: the home folder.
- `HEIMDALL_BUNDLE_ID`: the edition's app ID, which picks the pointer. Without it a binary reads the Info.plist of the
  app it sits in, else assumes the personal edition.

Offline modes (replay, idle, diagnostics, hot-swap, lab scenarios) keep their isolated folders, never apply a
move and do not export `HEIMDALL_DATA`.

Other overrides: `HEIMDALL_DROPS_DIR` (drops), `HEIMDALL_BUG_INBOX` (bug notes; the personal edition's `bug_inbox`
setting names a code checkout's inbox, see `bugs.md`).

Floating Islands: the personal edition's helper keeps its own folder, `~/Library/Application Support/FloatingIslands`
(`FLOATING_ISLANDS_DIR` overrides it). The public edition keeps them in `<home>/.heimdall/floating-islands`: the app
exports that as `FLOATING_ISLANDS_DIR` at start, for the helper and the `islands` CLI the agent runs, so it never
shares the personal app's islands.
