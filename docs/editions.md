# Editions: public and personal

The repository builds two editions from one workspace. This export builds the public one, "Heimdall Alpha".

| | public | personal |
|---|---|---|
| Who | anyone building from source; the default | the developer's own install |
| Build | `scripts/bundle.sh` (or `--edition public`) | `scripts/bundle.sh --edition personal` |
| Output | `dist/public/Heimdall Alpha.app` | `dist/Heimdall.app` |
| Cargo | `--no-default-features --features app/public` (turns on `agent/public`, `listen/public`, `ui/public`) | default features |
| Voice | Phonon S only (English) | extra voices that are not published |
| Python | none (checked by `scripts/check_public_bundle.py`) | a Python voice runtime |
| Info.plist | `HeimdallEdition` = `public` | `HeimdallEdition` = `personal` |
| Docs in `Contents/Resources/docs` | `connectors.md` only | `docs/` as committed |
| Skills in `Contents/Resources/skills` | `public`, `shared` | personal sets, `shared` |

The agent's prompt names the bundled `docs/connectors.md` by path (`connectors_doc` in `crates/agent/src/harness.rs`).
Because docs ship inside the app, `scripts/bundle.sh` refuses to build with uncommitted changes under `docs/`.

The public edition is "Heimdall Alpha", bundle id `com.tobynoskillson.heimdall`, with its own Keychain names and its
own pointer folder (docs/home.md). `scripts/bundle.sh` reads the signing identity from `HEIMDALL_SIGN_IDENTITY` or
`git config heimdall.signingIdentity`; without one, use `--test-fixture` (below). The log line after
`Heimdall diagnostics started …` says `Heimdall edition: <name>`.

`edition::PUBLIC` (`crates/app/src/edition.rs`) is the switch in code. Run the edition tests with the public feature:
`cargo nextest run --no-default-features --features app/public`.

## Runners

Every runner is native code inside the app; nothing else is installed on the user's Mac.

| Part | Runner | Language |
|---|---|---|
| Speech to text (Parakeet) | Core ML, plus Heimdall's Metal encoder | Rust, Metal |
| Voice activity (Silero), Smart Turn, speech split | ONNX Runtime, linked in | Rust |
| Decider (d1) | `libd1v5_engine.dylib` + `mlx.metallib` (Decider V6, and V5 the rollback, on MLX) | Rust, MLX, Metal |
| Speakers (diarization, voice ID) | `heimdall-speakers-worker` (ggml, ONNX) | Rust, C++ |
| Voice (Phonon S) | `ptts`, in process | Rust |
| Memory search (EmbeddingGemma 2) | `heimdall-embedding-worker` (MLX Swift) | Swift, Metal |
| Agent (Pi) | bundled Node 26 running Pi 1.4.0 (`crates/agent/runtime/`) | Node (JavaScript) |
| Emergency cover CLI | `heimdall-emergency` | Rust |
| Islands | `FloatingIslands.app`, `islands` CLI | Rust (Swift fallback) |

The public edition refuses to bundle if any `.py`/`.pyc` file, python executable, `site-packages` or python script is
inside, if any Mach-O file loads a library outside macOS and the app, or if a personal-edition voice or turn model
is in its models, or if any file names the build machine's home folder (`scripts/check_public_bundle.py`, run by
`bundle.sh`; `bundle.sh` remaps those paths to `/build` with `--remap-path-prefix` and `-ffile-prefix-map`).

`scripts/bundle.sh --test-fixture` builds a TEST FIXTURE of the public edition for the VM tests: ad-hoc signed, no
models (an empty manifest), `HeimdallTestFixture` in its Info.plist, into `dist/test-fixture/`. It needs no signing
certificate and no models folder, so a clean public export can build it. It is never a release:
`scripts/package_release.sh` refuses a fixture, an ad-hoc signature and a bundle without models. See docs/install.md.

## What differs in the public edition

- Voice: Phonon S only, English only, and no instant acknowledgements.
- Turn detection: the stock Smart Turn v3.2 on the CPU.
- Prompts: `crates/agent/prompt/public/` (agent, worker, compaction, relay) and `crates/app/prompt/public/` (islands,
  planner, goals, suggester). They name no person, paths or private projects.
- Pi's working folder is the home folder the user picks (`home.md`); Heimdall's own files live in `<home>/.heimdall`.
- Speech vocabulary: `listen/src/public-vocabulary.json` (Heimdall, Claude, Codex, T3 Code).
- Bugs: no bug-note tool or inbox. The prompt's "Bugs in Heimdall" section offers to report a bug to
  `TobyNoSkillSon/Heimdall` through the user's own `gh`, each issue or pull request only with their yes (docs/bugs.md).
- No-prompt harness setup (docs/harness-autonomy.md): asked once in Setup's Coding Agents step (Set Up or Leave as
  Is, saved as `harness_autonomy`); nothing is touched until Set Up.
- Personal-only features are compiled out (`cfg(feature = "public")`) or not bundled.

## Known gaps

- Decider: Decider V6 is the default and V5 the rollback. The weights are not in this repository; they are published
  at https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6 (int8 default). They were fine-tuned partly on the
  developer's own speech, so wakes may be less accurate for other voices.
- The owner's speaker label in transcripts, the decider's input and some tool descriptions is a fixed internal name,
  not the user's configured name. The decider was trained on that label, so renaming it needs a decider check.
- Echo cancellation ships off; use headphones so Heimdall does not hear itself.
- T3 Code is required for delegation (`~/.t3`); without it, delegation answers "T3_UNAVAILABLE".
- Phonon S speaks English only; the decider's language guess covers English and Polish.
- Signing: a release needs a Developer ID certificate and notarisation; source builds are ad-hoc or self-signed.
- Memory search needs macOS 26 or newer on Apple silicon (its worker loads `libswiftCompatibilitySpan.dylib`, which
  macOS 26 provides).
- "Support the Developer…" in the menu opens tobynoskillson.com.
