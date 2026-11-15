# Home: where Heimdall lives

The user picks one folder as Heimdall's home in Setup. The agent works in it, and every file of Heimdall's own
(settings, logs, archive, models, agent state, drops, shots, clips) lives in its data folder, `<home>/.heimdall`.

Read this when: you need the path of Heimdall's files, you move the home folder, a child process or CLI must find the
data folder, or you run Heimdall on an isolated folder for a test.
Code: `crates/home/src/lib.rs` (`heimdall_home`: resolution, the pointer, moves), `crates/app/src/home_change.rs` (the
app's side: plan, request, relaunch, start).

## Where the files are

- **Home**: the folder chosen in Setup's step 8 ([setup.md](setup.md)); the default is the user's home folder. It is
  the agent's working folder, and the deny list allows deletes inside it ([harness-autonomy.md](harness-autonomy.md)).
- **Data folder**: `<home>/.heimdall` (`heimdall_home::DIR`). Every child process finds it as `$HEIMDALL_DATA`.
- **Pointer**: the only file outside home, `$HOME/Library/Application Support/<bundle id>/home.json`, one per edition
  (public `com.tobynoskillson.heimdall`, personal `dev.heimdall.app`), so the two editions never read each other's
  profile:

  ```json
  { "home": "/Volumes/Work/Notes" }
  ```

  While a move waits it also has `data` (where the files still are) and `pending` (`from`, `to_home`, `leave_link`).
- **Islands**: the public edition keeps its floating islands in `<data>/floating-islands`, exported as
  `FLOATING_ISLANDS_DIR` at start ([islands-capabilities.md](islands-capabilities.md)).

## How the location is found

First match wins (`heimdall_home::resolve_in`):

1. `HEIMDALL_DATA` (with `HEIMDALL_HOME`) from the environment.
2. The pointer.
3. In the personal edition only, its profile from before home folders.
4. Otherwise a fresh profile: home is the user's home folder, data is `$HOME/.heimdall`.

`Location::needs_choice()` is true for the last two; that is when Setup shows its home step. The start's log says
`Heimdall home: <home> (data <data>, <source>)`.

## Change the home folder

From Setup's home step or the menu's Home Folder…; an agent uses `heimdall setup --home PATH`.

1. `home_change::plan(new_home)` checks the folder and says what will happen, writing nothing. The folder must be an
   absolute path that exists, is a folder, is writable, is not inside the current data folder and has no non-empty
   `.heimdall` yet. The plan gives the size and whether it is one rename (same volume) or a copy.
2. `home_change::request(new_home)` records the move in the pointer, or just the new home when nothing exists yet.
   It is refused when the app was started on a named folder (`--data-dir`, `HEIMDALL_DATA`), so lab runs never move
   anything.
3. `home_change::relaunch()`, then the caller quits: a helper waits for this process to end and runs `open -n` on the
   app.
4. At the next start, before anything opens the data folder, `home_change::start` calls
   `heimdall_home::apply_pending`. On the same volume the move is one rename. Across volumes it copies to
   `.heimdall.partial`, compares entry counts and bytes, renames, points the pointer at it, then removes the old
   folder.
5. Check it worked: the log says `Heimdall home: moved: …`. A failure drops the pending move, leaves the files where
   they were and logs `Heimdall home: the move did not happen; the files stay where they were: …`.

## Environment

The app sets these at start, so every child (the agent and the CLIs it runs, workers, the emergency CLI, islands)
uses the same folders. The agent's environment is cleared and these are passed through by name
(`crates/agent/src/harness.rs`).

| Variable | Meaning |
|---|---|
| `HEIMDALL_DATA` | the data folder; `--data-dir` sets it for that run |
| `HEIMDALL_HOME` | the home folder |
| `HEIMDALL_BUNDLE_ID` | the edition's app ID, which picks the pointer; without it a binary reads the `Info.plist` of the app it sits in, else assumes the personal edition |
| `FLOATING_ISLANDS_DIR` | the islands folder (public edition) |
| `HEIMDALL_FOLDERS_EXPORTED`, `HEIMDALL_ISLANDS_EXPORTED` | marks that the app exported the variables above; a relaunched app drops marked variables before it reads anything, while a `HEIMDALL_DATA` set by hand or by a test still wins |

- Offline modes (replay, idle, night diagnostics, hot-swap, lab scenarios) keep their isolated folders, never apply a
  move and do not export `HEIMDALL_DATA`.
- A cargo test binary that names no folder gets its own temporary data folder, never the user's profile
  (`HEIMDALL_TEST_REAL_PROFILE=1` opts out for a lab that must read the real one).
- `HEIMDALL_DROPS_DIR` overrides where drops go.

## Tests

```sh
cargo test -p heimdall-home
cargo test -p app home_change
```

`crates/home/src/tests.rs` covers resolution, the pointer and moves; `crates/app/src/home_change_tests.rs` covers the
app's plan, request and start.

## See also

- [setup.md](setup.md): the home step in Setup
- [harness-autonomy.md](harness-autonomy.md): deletes the deny list allows inside home
- [archive.md](archive.md): what the archive keeps in the data folder
- [editions.md](editions.md): the two editions and their app IDs
