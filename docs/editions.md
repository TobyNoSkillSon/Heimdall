# Editions: public and personal

Heimdall builds as two editions from one source tree. The public edition, Heimdall Alpha, is the release for other
people's Macs: one built-in voice, no Python, large models downloaded on first run. The personal edition is the
owner's build, with every model bundled and a few features of its own.

Read this when: you want to know what a build contains, add or gate an edition-specific feature, change what the
public bundle may hold, or build a test fixture.
Code: `crates/app/src/edition.rs` (`edition::PUBLIC`, `NAME`, `BUNDLE_ID`, `VERSION`), `scripts/bundle.sh`,
`scripts/bundle_models.py`, `scripts/check_public_bundle.py`.

## What each edition is

| | Public | Personal |
|---|---|---|
| Build | `scripts/bundle.sh` (the default) or `--edition public` | `scripts/bundle.sh --edition personal` |
| Output | `dist/public/Heimdall Alpha.app` | `dist/Heimdall.app` |
| Name, bundle ID | Heimdall Alpha, `com.tobynoskillson.heimdall` (`heimdall_home::PUBLIC_ID`) | Heimdall, `dev.heimdall.app` (`heimdall_home::PERSONAL_ID`) |
| Cargo | `--no-default-features --features app/public` | default features (`friends`): plain `cargo build`, `cargo test` |
| Built-in voice | Phonon S only | Drama 3 (default), Supertonic, Phonon S |
| Models | small ones bundled; the rest download on first run ([models.md](models.md)) | all bundled |
| Python | none | the Drama 3 voice runtime |
| Info.plist `HeimdallEdition` | `public` | `personal` |
| `Contents/Resources/docs` | `docs/` as committed, with crate READMEs and `islands/` docs at their repository paths | the same, plus `docs/personal/` |
| `Contents/Resources/skills` | `skills/public`, `skills/shared` | the same |
| Updates | in-app updater ([updates.md](updates.md)), off in this alpha: Homebrew or a new download | none |
| Signing identity | `HEIMDALL_PUBLIC_SIGN_IDENTITY` or `heimdall.publicSigningIdentity`, else the personal one | `HEIMDALL_SIGN_IDENTITY` or `heimdall.signingIdentity` |

- `app/public` turns on `agent/public`, `listen/public`, `t3/public` and `ui/public`, and leaves out `friends`, so iroh
  is not built. `agent/public` turns on `heimdall-emergency/public`.
- Both editions share one version, the workspace's (`Cargo.toml` `[workspace.package]`).
- Each edition has its own Keychain item names (public items start with "Heimdall Alpha"), its own pointer folder
  ([home.md](home.md)) and its own permission grants, so the two never read each other's keys or profile on one Mac.
- The log line after `Heimdall diagnostics started …` reads `Heimdall edition: <name>`.
- Docs ship inside the app, so `scripts/bundle.sh` refuses to build with uncommitted changes under `docs/`. The agent's
  prompt names the bundled `docs/connectors.md` (`connectors_doc` in `crates/agent/src/harness.rs`) and the docs
  folder (`docs_dir`), whose `README.md` is the index.
- The agent's prompt and skills are the same general text in both editions. What one user's install has, and their own
  rules and skills, come from their data folder at runtime ([skills.md](skills.md), "Two layers").

## Gate a feature by edition

1. In the app crate, branch on `edition::PUBLIC` (a `const bool`). In another crate, add a `public = []` feature to its
   `Cargo.toml`, add it to `app`'s `public` list, and use `cfg(feature = "public")`.
2. Keep the public path compiling and tested: the check script runs the edition tests again with the public feature
   (the `public-edition` step; [contributing.md](contributing.md)).
3. If the feature adds bundled files, decide in `scripts/bundle_models.py` (models) or `scripts/bundle.sh` (other
   resources) whether the public bundle takes them.
4. Check: `scripts/bundle.sh --edition public` passes `scripts/check_public_bundle.py`.

## What the public edition changes

| Area | Public edition | Where |
|---|---|---|
| Voice | Phonon S. A saved Drama 3 or Supertonic choice speaks with Phonon S; the voice-bank fill does not start | `crates/app/src/speech_voice.rs`, `voice_bank.rs` |
| Speaking menu | Drama 3 only where its model is installed; Supertonic only while it is the saved choice | `crates/ui/src/mac/speaking_menu.rs` |
| Turn detection | the stock Smart Turn on the CPU (`turn_backend` default `cpu`) | `crates/app/src/settings.rs` |
| Agent prompt, task prompt and compaction text | the general text, as in the personal edition | `crates/agent/prompt/` |
| Islands planner prompt | the public version | `crates/app/prompt/public/` |
| Speech vocabulary | a short generic list | `crates/listen/src/public-vocabulary.json` |
| Agent model, background model, pre-added models | nothing chosen until Setup or the menu picks one | `AgentModel::Unchosen`, `BackgroundModel::default` |
| Pi's working folder | the home folder the user picks; Heimdall's files in `<home>/.heimdall` | [home.md](home.md) |
| No-prompt harness setup | asked once in Setup (`harness_autonomy`); nothing changes until Set Up | [harness-autonomy.md](harness-autonomy.md) |
| Arsenal | offered in Setup; when chosen, the path saved in `arsenal_mcp` is registered as the `arsenal` MCP server per session by `crates/agent/runtime/arsenal.mjs` | [setup.md](setup.md) |
| Bug reports | the `report-bug` skill offers to file a GitHub issue through the user's own `gh`, only with their yes | `skills/public/report-bug/SKILL.md` |
| Approval grants | off (`grants::ENABLED`) | `crates/emergency-cli/src/grants.rs` |
| Personal-only features | compiled out or never started; `mcp_contract.rs` marks personal-only tools | `cfg(feature = "public")`, `edition::PUBLIC` |

## What the public bundle may contain

`scripts/check_public_bundle.py`, run by `scripts/bundle.sh` for every public build, fails the bundle when:

- any `.py` or `.pyc` file, Python executable, `site-packages` folder or script whose `#!` names Python is inside;
- any Mach-O file loads a library outside macOS (`/System/Library`, `/usr/lib`) and the bundle, or searches `@rpath`
  outside them;
- a Drama 3, Supertonic or tuned Smart Turn asset is in its models;
- `HeimdallEdition` is not `public`;
- any file names the build machine's home folder. `scripts/bundle.sh` remaps it to `/build` with
  `--remap-path-prefix` and `-ffile-prefix-map`.

## Runners

Every runner is native code inside the app; nothing else is installed on the user's Mac.

| Part | Runner | Language |
|---|---|---|
| Speech to text (Parakeet) | Core ML, plus Heimdall's Metal encoder | Rust, Metal |
| Voice activity (Silero), Smart Turn, speech separation | ONNX Runtime, linked in | Rust |
| Decider | `libd1v5_engine.dylib` and `mlx.metallib` | Rust, MLX, Metal |
| Speakers (diarization, voice ID) | `heimdall-speakers-worker` (ggml, ONNX) | Rust, C++ |
| Voice (Phonon S) | in process | Rust |
| Memory search (EmbeddingGemma 2) | `heimdall-embedding-worker` (MLX Swift) | Swift, Metal |
| Agent (Pi) | bundled Node 26.8.1 running Pi 1.4.0 (`crates/agent/runtime/`) | JavaScript |
| Emergency cover CLI | `heimdall-emergency` | Rust |
| Islands | `FloatingIslands.app`, the `islands` CLI | Rust |
| Built-in engine | `heimdall-splash` and its native engine ([inference-engine.md](inference-engine.md)) | Rust, Objective-C++, Metal |

## Build a test fixture

`scripts/bundle.sh --test-fixture` builds the public edition ad-hoc signed, with an empty models manifest and
`HeimdallTestFixture` in its Info.plist, into `dist/test-fixture/Heimdall Alpha.app`. It needs no certificate and no
`models/` folder, so a clean checkout can build what the VM tests install. Listening, the decider, speakers, memory
search and the voice cannot run in it. `scripts/package_release.sh` refuses a fixture, an ad-hoc signature and a bundle
without models.

## Known limits of the public edition

- The app is not notarised; a release needs a Developer ID certificate for that.
- Delegation needs T3 Code; without it the T3 tools answer `T3_UNAVAILABLE` ([t3-orchestration.md](t3-orchestration.md)).
- Phonon S speaks English only, and the decider's language guess covers English and Polish.
- The memory-search reranker downloads at first run, like the other large models; without it search keeps embedding
  order ([chunk-index.md](chunk-index.md)).
- The built-in engine is off by default, and its sizes in `crates/app/config/inference-engine.json` are tuned for a
  128 GB Mac.
- The speaker label for the Mac's owner is a fixed internal name that the decider was trained on; the public prompt
  tells the agent it is a label. Several tool descriptions and decider questions use the same name.
- "Support the Developer…" in the menu opens the developer's website.

## See also

- [install.md](install.md): building, installing and rolling back a build
- [updates.md](updates.md): the public edition's updater and releases
- [models.md](models.md): first-run downloads and model settings
- [home.md](home.md): the home folder, the data folder and the pointer
