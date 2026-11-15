# Build, install and roll back Heimdall

How to build a signed Heimdall app from this repository, install it, keep one rollback, uninstall it and restore the
previous version. Installing, launching, resetting permissions and deleting data are separate actions: an agent does
only the ones the user asked for.

Read this when: you want to build the app from source, install or replace a build, go back to the previous build,
uninstall Heimdall, or delete its data.
Code: `scripts/bundle.sh`, `scripts/bundle_models.py`, `scripts/verify_release_candidate.py`.

## Editions and where each build goes

- Public, the default: `scripts/bundle.sh` (or `--edition public`) builds `dist/public/Heimdall Alpha.app`.
- Personal: `scripts/bundle.sh --edition personal` builds `dist/Heimdall.app`.

`--edition both` builds personal, then public. Each edition's contents, bundle ID and data folder:
[editions.md](editions.md). A build from source needs the model assets of its edition (below); the personal edition's
voice models are not public, so a build from a public checkout is the public edition.

## Before the first build

Tools, once per Mac:

1. An Apple Silicon Mac. The app runs on macOS 14.4 or later (`LSMinimumSystemVersion`); building needs the macOS that
   your Xcode requires.
2. Xcode 26 or later from the App Store, selected and set up:

   ```sh
   sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
   sudo xcodebuild -license accept
   sudo xcodebuild -runFirstLaunch
   xcodebuild -downloadComponent MetalToolchain   # Xcode 26+ ships without the Metal compiler; about 840 MB
   ```

   Why Xcode and not the Command Line Tools: the speakers compiler lock names Xcode's clang, the memory-search helper
   builds with Xcode's Swift (`xcrun swift`), and the Metal compiler is Xcode's.
3. [Homebrew](https://brew.sh), then the build tools: `brew install cmake python ffmpeg`. CMake builds MLX for the
   decider engine; Python 3.11 or later runs the build scripts (macOS's own `python3` is older); ffmpeg cuts the
   speaker tests' meeting clip in `crates/speakers/scripts/fetch_models.py`. Nothing Python ships in the app.
4. Gotcha: installing Homebrew installs the Command Line Tools and selects them. Run the `xcode-select -s` line of step
   2 again afterwards.
5. Rust from [rustup](https://rustup.rs), in `$HOME/.cargo/bin`.
6. Check: `xcode-select -p` prints `/Applications/Xcode.app/Contents/Developer`, `xcrun metal --version` and
   `cmake --version` answer, and `python3 --version` says 3.11 or later.

Signing:

- The public edition can be built without a certificate: `scripts/bundle.sh --adhoc` signs it ad hoc. macOS then ties
  the microphone, screen and accessibility grants to that one build, so they may need granting again after each
  rebuild or update.
- With a code-signing identity in the login keychain, `scripts/bundle.sh` reads its SHA-1 from
  `HEIMDALL_SIGN_IDENTITY` or `git config heimdall.signingIdentity`; the public edition may use its own
  (`HEIMDALL_PUBLIC_SIGN_IDENTITY` or `git config heimdall.publicSigningIdentity`, else the same one).
  `security find-identity -v -p codesigning` lists the SHA-1s. A signed build keeps the permission grants across
  rebuilds. `--test-fixture` builds an ad-hoc app with no models, for UI checks only ([editions.md](editions.md)).

Model assets, in `models/` at the repository root. `scripts/bundle_models.py` copies only files whose SHA-256 matches a
lock, and stops on a missing or changed one. Fill `models/` in this order:

1. `python3 crates/listen/scripts/fetch_models.py` (speech, voice activity, Smart Turn).
2. `python3 crates/speak/scripts/fetch_phonon.py` (Phonon S).
3. `python3 crates/speakers/scripts/fetch_models.py` (speaker naming; needs ffmpeg).
4. `scripts/build_decider_v5.sh --lock` (the decider's MLX engine and metallib). Its MLX sources come from
   `crates/decide/engine-v5/build/fetch-deps.sh`, which copies a local MLX build when one exists and otherwise clones
   the pinned tags and downloads metal-cpp; a first run also fetches the engine's Rust crates (`cargo fetch`).
5. `python3 scripts/build_embedding_native.py` (the memory-search helper).
6. Commit the rewritten locks: `git add crates/decide/scripts/runtime-v5.lock.json
   crates/archive/embedding/assets.lock.json && git commit -m "Locks for this build"`. Why: the decider engine and
   the embedding helper embed their build folder's paths and the compiler's version, so a build on another Mac or in
   another checkout has another SHA-256 than the committed lock, and `scripts/bundle.sh` refuses uncommitted changes.

Build jobs: the decider engine and the embedding helper build at nice 19 with two jobs; `HEIMDALL_JOBS=<n>` gives
them more. `scripts/bundle.sh`'s cargo builds use every core unless `CARGO_BUILD_JOBS=<n>` says otherwise.

The built-in engine's files come from a verified cache filled by `scripts/bundle_splash.sh` from the pins in
`scripts/splash-engine.lock.json` ([inference-engine.md](inference-engine.md)).

The public edition downloads its large models on first run instead of bundling them ([models.md](models.md)).

## Build a signed app

1. Commit everything. `scripts/bundle.sh` refuses uncommitted changes in `crates/ui` and any tracked diff under
   `crates`, `scripts`, `islands`, `skills`, `docs`, `Cargo.toml` or `Cargo.lock`, and checks again after the build.
   Why: the docs, skills and source commit ship inside the app.
2. Run `scripts/bundle.sh` (add `--edition personal` for the personal edition, or `--adhoc` for a public build
   without a certificate). `CARGO_TARGET_DIR` defaults to `target/release-candidate`.
3. Check the line `Signed bundle (<edition>): <path>` at the end, then
   `/usr/bin/codesign --verify --deep --strict "dist/public/Heimdall Alpha.app"`.

What the script guarantees:

- Every nested executable and dylib is signed with the edition's identity. The models manifest
  (`Contents/Resources/models/manifest.json`) records each file's source hash and its sealed hash after signing.
- `Contents/Resources/source-commit.txt` holds the commit the app was built from.
- The public edition fails the build if `scripts/check_public_bundle.py` finds Python, a library outside macOS and the
  app, an excluded model or the build machine's home path ([editions.md](editions.md)).
- The designated requirement must match the previous build at the output path, and for the personal edition the
  installed `$HOME/Applications/Heimdall.app`; a mismatch leaves both untouched. Why: the same requirement keeps macOS
  permission grants and Keychain access.

`scripts/bundle.sh --probe` builds the capture probe in place of the app (personal unless `--edition` says otherwise;
[capture.md](capture.md)).

## Verify a personal-edition candidate

`scripts/verify_release_candidate.py --evidence dist/<new folder> [--bundle dist/Heimdall.app] [--lab DIR]` checks a
personal-edition build only: it refuses any `HeimdallEdition` other than `personal`, and checks every nested
certificate against the identity pinned in the script (`IDENTITY`). It verifies deep and strict signatures, every
manifest row and the night assets, then runs the bundled executable with `--night-replay-smoke` against a fresh data
folder (`--lab`, default `$CARGO_TARGET_DIR/candidate-lab-<commit>`): fake capture sources, dry-run actions, a 220 s
deadline. It never opens the UI, starts capture, calls a provider or requests permissions. It proves the bundle
launches and its resources load, not live capture or permission grants.

## Quit Heimdall before replacing or removing it

1. If **Launch at Login** is checked in Heimdall's menu, uncheck it. The app calls
   `SMAppService.mainAppService unregisterAndReturnError:` and saves `launch_at_login=false` only on success. On an
   error, stop. There is no LaunchAgents plist and no login CLI flag to use instead.
2. Choose **Quit Heimdall** in the same menu (**Quit <name>** after a rename, [personalise.md](personalise.md)). Only
   this drains the pipeline and the archive; an AppleScript quit, SIGTERM or `kill` does not. Drive it through
   accessibility or ask the user, never through the user's own pointer.
3. Check that nothing still runs the installed executable (exit code 1 means none):

   ```bash
   /usr/bin/pgrep -fl "Heimdall Alpha.app/Contents/MacOS/Heimdall"   # "Heimdall.app/…" for the personal edition
   ```

   Inspect a match instead of killing it.

If the app is already stopped but still registered at login, turn it off in System Settings > General > Login Items &
Extensions, and unregister natively at the next launch the user allows. Do not launch the app only to uninstall it.

## Install or replace

Install into `/Applications` or `$HOME/Applications` under the bundle's own name (`Heimdall Alpha.app` or
`Heimdall.app`). Gotcha: the app keeps provider keys in the login Keychain only when it runs from one of those two
paths (`installed_at` in `crates/agent/src/provider.rs`); a copy run from anywhere else keeps keys in memory until it
quits.

1. Quit Heimdall as above.
2. Compare designated requirements. Run both commands on the new build and, if one is installed, on the installed app:

   ```bash
   /usr/bin/codesign --verify --deep --strict "$NEW_APP"
   /usr/bin/codesign -d -r- "$NEW_APP"
   ```

   The `designated =>` lines must be identical. On a difference, stop: do not ad-hoc sign, change certificates or reset
   permissions to make it pass. An `--adhoc` build has no stable requirement to compare: skip this step for it, and
   expect macOS to drop the old build's microphone, screen and accessibility grants. At its next launch Heimdall asks
   for the microphone again, or shows a red pill that opens the System Settings pane of a grant it lost
   ([setup.md](setup.md#after-an-update)).
3. Run the block below with `HEIMDALL_REPO` set to the checkout, `APP_NAME` to `Heimdall Alpha` (public) or `Heimdall`
   (personal), and `DEST_DIR` to `/Applications` or `$HOME/Applications`. It stages a fresh copy, swaps it in and keeps
   exactly one rollback, `dist/rollback/previous/<APP_NAME>.app`. It does not launch anything.

   ```bash
   set -euo pipefail
   source_app="$HEIMDALL_REPO/dist/public/$APP_NAME.app"   # dist/$APP_NAME.app for the personal edition
   destination="$DEST_DIR/$APP_NAME.app"
   rollback="$HEIMDALL_REPO/dist/rollback"
   /bin/mkdir -p "$DEST_DIR" "$rollback"
   stage=$(/usr/bin/mktemp -d "$DEST_DIR/.heimdall-install.XXXXXX")
   /usr/bin/ditto "$source_app" "$stage/$APP_NAME.app"
   /usr/bin/codesign --verify --deep --strict "$stage/$APP_NAME.app"
   retained=""
   if [[ -e "$destination" ]]; then
     /usr/bin/codesign --verify --deep --strict "$destination"
     retained=$(/usr/bin/mktemp -d "$rollback/incoming.XXXXXX")
     /bin/mv "$destination" "$retained/$APP_NAME.app"
   fi
   /bin/mv "$stage/$APP_NAME.app" "$destination"
   /bin/rmdir "$stage"
   /usr/bin/codesign --verify --deep --strict "$destination"
   if [[ -n "$retained" ]]; then
     /bin/rm -rf "$rollback/previous"
     /bin/mv "$retained" "$rollback/previous"
   fi
   ```

   If the final move fails, move `$retained/$APP_NAME.app` back to the empty destination before retrying. An
   interrupted stage is left for inspection. Why a fresh stage: `ditto` onto an existing bundle merges stale files into
   a signed app.
4. Check: `/usr/bin/codesign --verify --deep --strict "$destination"` passes and `dist/rollback/previous/` holds the
   replaced version.

After the swap, before the new build starts:

- Skills: an existing profile keeps its own copies of bundled skills ([skills.md](skills.md)). When `skills/` changed
  since the installed build (`git diff --stat <old>..<new> -- skills/`), copy the changed skills as
  [skills.md](skills.md) describes, or ask Heimdall to `reload_me` after it starts.
- Claude Code permission hook, only if the user agreed to it:
  `"$destination/Contents/MacOS/heimdall-emergency" hook install`. It replaces an earlier entry in
  `$HOME/.claude/settings.json`, so it is safe at every install and keeps the hook on the installed app
  ([harness-autonomy.md](harness-autonomy.md)).
- No-prompt harness setup: Heimdall reapplies it at each start where the user chose it. To apply it now,
  `"$destination/Contents/MacOS/heimdall-emergency" harness apply`; `harness check` only reports
  ([harness-autonomy.md](harness-autonomy.md)).
- Relay MCP server, only if the user agreed to it: `"$destination/Contents/MacOS/heimdall-emergency" mcp install`
  writes Heimdall's server into each Claude Code user config, replacing an earlier entry; running Claude sessions pick
  it up when they next start. `mcp uninstall` removes it ([claude-relay.md](claude-relay.md)).

Launch only when the user asked for it: `/usr/bin/open "$destination"`. The user grants any permission macOS asks
for. Turn Launch at Login back on only if they want it.

A self-built public app checks for updates like a release, but a published release is signed by another certificate,
so its download fails the designated-requirement check and nothing changes ([updates.md](updates.md)).

## Roll back to the previous build

1. Verify `dist/rollback/previous/<APP_NAME>.app` with `codesign --verify --deep --strict` and compare its designated
   requirement with the installed app's, as in step 2 above.
2. Quit Heimdall and unregister Launch at Login.
3. Run the install block with `source_app` set to that bundle's absolute path. The two builds swap, so there is still
   exactly one rollback. Never re-sign a retained bundle.

Settings across versions: a build ignores unknown fields in `settings.json` at every depth and rejects invalid values
of known ones, so an older build loads a newer file. But a save by the older build drops the keys it does not know:
copy `settings.json` before rolling back and restore the copy when you return to the newer build
([settings.md](settings.md)). A rollback does not roll back data schemas or restore deleted data.

An archive with `archive/session-*/` folders from before the single store is migrated by the running app, or at once
with `heimdall-archive migrate` ([archive.md](archive.md)).

## Uninstall and keep the data

1. Quit Heimdall and unregister Launch at Login.
2. Keep a verified copy of the installed bundle under `dist/rollback/` (`mktemp -d`, `ditto`,
   `codesign --verify --deep --strict`) and note its path.
3. Remove the exact bundle, for example `/bin/rm -rf "/Applications/Heimdall Alpha.app"`.

This keeps all data: the data folder `<home>/.heimdall` (settings, agent state, models, and the archive store
`archive/heimdall.sqlite` with `archive/packs/`) and the pointer folder under `$HOME/Library/Application Support/`
([home.md](home.md)). It does not revoke permission grants. Never delete single archive files or packs: the index
references every pack.

## Delete data (only with the user's explicit consent)

With the app and its workers stopped:

- The archive only: `/bin/rm -rf "<home>/.heimdall/archive"`.
- All Heimdall data, including settings and agent state, needs separate consent for that wider scope:
  `/bin/rm -rf "<home>/.heimdall"` and the edition's pointer folder, for example
  `/bin/rm -rf "$HOME/Library/Application Support/com.tobynoskillson.heimdall"`.

Deletion cannot be undone by restoring a bundle. Make any backup the user wants first, then check the paths are gone.

## Reset permissions (only when asked)

A permission reset is never needed for a signed update or rollback. With Heimdall stopped, reset this bundle only,
for example `/usr/bin/tccutil reset All com.tobynoskillson.heimdall`. The next launch may ask for every permission
again. Never omit the bundle ID, and never run a reset to test the syntax.

## See also

- [editions.md](editions.md): what the public and personal builds contain
- [updates.md](updates.md): the public edition's in-app updater, releases and its own rollback
- [contributing.md](contributing.md): running the tests and the full check before a build
- [home.md](home.md): the data folder and the pointer
- [resilience.md](resilience.md): when the installed app fails
