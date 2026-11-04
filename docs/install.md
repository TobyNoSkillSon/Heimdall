# Build, install and uninstall Heimdall Alpha

Heimdall Alpha is the public edition. There is no prebuilt release yet: you build it from source. It needs an Apple
silicon Mac; memory search needs macOS 26 or newer.

## What you need

- Xcode command line tools: `xcode-select --install`. The full Xcode is needed only to sign with your own Apple
  Development certificate.
- The Rust toolchain from https://rustup.rs (stable).
- `cargo-nextest` for the tests: `curl -LsSf https://get.nexte.st/latest/mac | tar zxf - -C ~/.cargo/bin`.
- Python 3 for the model download scripts (build time only; nothing Python ships in the app).
- About 5 GB for models and 20 GB for the build.
- T3 Code, if you want Heimdall to hand work to coding agents (docs/t3-orchestration.md).

## Models

`scripts/fetch_models.sh` downloads every model the app bundles into `models/`, each pinned to a revision and checked
against its SHA-256 (about 4.5 GB), then builds the native embedding helper. It is safe to run again. The decider
(Decider V6, int8) comes from https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6 through `scripts/fetch_decider.sh`;
its weights are not in this repository.

## Build

```bash
scripts/fetch_models.sh
scripts/bundle.sh
```

`scripts/bundle.sh` builds the public edition into `dist/public/Heimdall Alpha.app`. It refuses to build with
uncommitted changes under `crates`, `scripts`, `islands`, `skills` or `docs`, and records the commit in
`Contents/Resources/source-commit.txt`.

Signing: the build signs every executable with one code-signing identity, given as its SHA-1 in
`HEIMDALL_PUBLIC_SIGN_IDENTITY` (or `git config heimdall.publicSigningIdentity <SHA-1>`). A free Apple Development
certificate from Xcode works; `security find-identity -v -p codesigning` lists yours. Keep using the same one: macOS
ties the app's permissions to it, and a new identity means granting them again.

Without a certificate, `scripts/bundle.sh --test-fixture` builds an ad-hoc signed test fixture into
`dist/test-fixture/`, with no models. It is for trying the build and running the VM tests, not for daily use.

What the public edition bundles, and how it differs from the personal one: `docs/editions.md`.

## Full check

`scripts/check.sh --tests` runs every workspace test and doctest, the script tests and the public-edition gates. Use
it rather than a hand-made `cargo nextest` command: it puts test temp files on a private RAM disk and keeps one build
cache, `target/test`.

- `scripts/check.sh --change` tests only the crates your change can reach, compared with `main`.
- `scripts/check.sh --status` prints the run in progress or the last result.
- Each run keeps its logs in `target/check/<time>-<mode>/` (`target/check/latest` is the newest).
- Steps run at nice 19 with two build jobs; `HEIMDALL_JOBS` overrides.

The default tests run no model inference.

## Install

1. If an older copy is running, choose **Quit Heimdall** in its menu (a plain kill skips the archive drain), and
   turn off **Launch at Login** first if you plan to remove it.
2. Copy `dist/public/Heimdall Alpha.app` to `/Applications` or `~/Applications`. Keep the previous copy if you want a
   rollback.
3. Open it. Setup asks for Microphone, System Audio, Screen Recording and Accessibility, a home folder, an agent
   provider (docs/connectors.md), the decider size, T3 Code and, optionally, Vella.

Heimdall records your screen and transcribes everyone it hears, in the room and in calls. All of it stays on your
Mac, in your home folder's `.heimdall` (docs/home.md).

## Uninstall

1. Turn off **Launch at Login**, then **Quit Heimdall** from its menu.
2. Delete the app bundle.
3. Your data stays until you delete it: the home folder's `.heimdall` (settings, agent state and the archive of
   audio, transcripts and screen) and `~/Library/Application Support/com.tobynoskillson.heimdall/home.json`. Deleting
   them cannot be undone.
4. To clear macOS permissions too: `tccutil reset All com.tobynoskillson.heimdall`.

## Settings across versions

Builds ignore unknown `settings.json` fields and reject invalid values of known ones, so an older build opens a newer
file. An older build drops keys it does not know when it saves, so copy `settings.json` before going back a version.
A rollback does not roll back data schemas.
