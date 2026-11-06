<p align="center">
  <img src="docs/images/icon.png" alt="" width="88">
</p>

<h1 align="center">Heimdall <img src="docs/images/alpha.svg" alt="Alpha" width="58" height="24"></h1>

<p align="center">An assistant you talk to that sees and hears your Mac, instead of a chat window.</p>

<p align="center">
  <a href="#install-the-alpha"><img src="docs/images/download.svg" alt="Download" width="152" height="42"></a>
  &nbsp;
  <a href="https://tobynoskillson.com/heimdall"><img src="docs/images/website.svg" alt="Website" width="140" height="42"></a>
  &nbsp;
  <a href="https://tobynoskillson.com/#support"><img src="docs/images/support.svg" alt="Support the Developer on GitHub Sponsors or Buy Me a Coffee" width="228" height="42"></a>
</p>

<p>
<picture>
  <source media="(max-width: 600px)" srcset="docs/images/readme-hero-phone.webp">
  <img src="docs/images/readme-hero.webp" alt="A Mac desktop with Heimdall: the notch says the build is green, Goals and Scheduled cards on the left, a fine-tune job and a trip question on the right">
</picture>
</p>

Say it out loud. Heimdall hears you without a wake word, hands the job to an agent, and shows it as a small card on your
screen. It asks only when it must.

<p align="center">
  <img src="docs/images/island-site.webp" alt="A landing page job card: the page's picture, its score and two judges' lines" width="359">
  <img src="docs/images/island-film.webp" alt="A film restore job card: an 1897 frame, 212 of 480 frames done" width="359">
</p>

<p>
  <img src="docs/images/readme-islands.webp" alt="The Today timeline and the Settled card: what got done this week">
</p>

> **Alpha (v0.1.0-alpha.1).** This is an early build of the developer's own daily assistant. Expect rough edges,
> features marked beta below, and changes without migration. 

## Install the alpha

With [Homebrew](https://brew.sh):

```sh
brew install --cask tobynoskillson/tap/heimdall-alpha
```

Or download `Heimdall-Alpha-0.1.0-alpha.1-arm64.zip` from the
[v0.1.0-alpha.1 release](https://github.com/TobyNoSkillSon/Heimdall/releases/tag/v0.1.0-alpha.1), unzip it and move
`Heimdall Alpha.app` to Applications. The app is ad-hoc signed, not notarised: open it once, then click **Open Anyway**
in System Settings → Privacy & Security, or run `xattr -dr com.apple.quarantine "/Applications/Heimdall Alpha.app"`
(Homebrew clears the download flag for you).

Setup walks you through the rest: your name, the agent provider, the macOS permissions and the model downloads (about
3.8 GB, each file checked against its pinned SHA-256). The full guide, also written for an agent setting Heimdall up
for you: [tobynoskillson.com/heimdall/docs](https://tobynoskillson.com/heimdall/docs/).

Updates: `brew upgrade --cask heimdall-alpha`, or download the newer zip. The in-app updater is off in this alpha.

To build it yourself instead, see [Build and install](#build-and-install).

## Privacy, read this first

Heimdall records. With listening and screen capture on it transcribes everyone in the room and on your calls, stores
voiceprints of the people it hears, and archives what is on your screen, including messages and anything else that
appears there. All of that stays on your Mac, in `<your home folder>/.heimdall`. What leaves the Mac: the text,
screenshots and recent conversation the agent needs, sent to the provider you connect (Claude, ChatGPT or OpenCode
Go). Tell the people around you, and check the recording laws where you live.

## Requirements

- A Mac with Apple silicon and macOS 14.4 or later; **macOS 26 recommended** (memory search needs it). 32 GB of RAM
  recommended; the models use about 4 GB.
- About 6 GB of free disk for the models, plus about 25 GB for the Rust build.
- Xcode 26 (for Swift, Metal and the macOS SDK), Rust (stable, via [rustup](https://rustup.rs)), CMake, and Python 3.11+
  (build scripts only; nothing Python ships in the app).
- Optional: an Apple Development signing certificate (free with any Apple Account in Xcode → Settings → Accounts).
  macOS ties microphone, screen and accessibility permissions to the signature, so a signed build keeps them across
  rebuilds.
- One agent provider: a Claude subscription through T3 Code, a ChatGPT
  subscription, an OpenCode Go subscription, or an Anthropic or OpenRouter API key.

## Build and install

```sh
git clone https://github.com/TobyNoSkillSon/Heimdall.git
cd Heimdall

# 1. Models (about 4.5 GB, checked against pinned SHA-256 hashes; see "Models" below)
scripts/fetch_models.sh

# 2. Build the app, ad-hoc signed: dist/public/Heimdall Alpha.app
scripts/bundle.sh --adhoc
```

With a certificate, `git config heimdall.signingIdentity <SHA-1>` and run `scripts/bundle.sh` instead
(`security find-identity -v -p codesigning` lists the SHA-1).

An ad-hoc build is unsigned as far as macOS is concerned: open it once, then click **Open Anyway** in System Settings →
Privacy & Security; the permissions may need granting again after each rebuild.

Copy `dist/public/Heimdall Alpha.app` to `/Applications` and open it. The setup wizard asks for a home folder (Heimdall
keeps its own files in `<home>/.heimdall`), walks through the microphone, screen recording and accessibility
permissions, and connects an agent provider. It starts blank: no profile, no voices, no history.

To check a clone compiles and passes its tests without models or a certificate:

```sh
cargo build --workspace --features app/public
cargo test --workspace --features app/public
```

`scripts/check.sh --tests` runs the full suite the way the project does (needs `cargo-nextest`). `scripts/bundle.sh
--test-fixture` builds an ad-hoc-signed public app with no models, for UI checks only.

## Models

Nothing here ships model weights in git. `scripts/fetch_models.sh` downloads each one from its source at a pinned
revision into `models/`, and `scripts/bundle.sh` copies them into the app.

| Part | Model | Size | Licence |
|---|---|---|---|
| Wake decision | Decider V6, 8-bit ([Toby-NoSkillSon/heimdall-d1-v6](https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6), fine-tuned from LiquidAI d1-2B) | 2.1 GB | LFM Open License 1.0 |
| Speech to text | Parakeet Ultra, Core ML (FluidInference/parakeet-ultra-coreml) | 0.6 GB | CC BY 4.0 |
| Memory search | EmbeddingGemma 2 (google/embeddinggemma-2) | 1.5 GB | Apache-2.0, Gemma prohibited-use policy |
| Speaker naming | Nemotron 3 Diarization, ReDimNet2 B6 | 0.15 GB | OpenMDW-1.1, MIT |
| Voice | Phonon S (gradium/phonon-31m-en) | 0.07 GB | Apache-2.0 |
| Voice activity, end of turn | Silero VAD, Smart Turn v3.2 | small | MIT, BSD-2-Clause |
| Voice separation | MossFormer2 (ipsilondev/MossFormer2-SS-16K-ONNX) | small | Apache-2.0 |

The Decider's licence allows commercial use only below USD 10 million in annual revenue. Full texts are in
`licenses/`; `NOTICE` and `THIRD_PARTY_NOTICES.md` list every bundled component.

## Alpha: what works and what is beta

Works:

- **Listening and transcription** of the microphone and system audio, live.
- **Suggestions**: one card offered from the last ten minutes of conversation.
- **Drawing and pointing** on your screen, and drawing for it.
- **Fast reading** and the **MCP tools** the agent uses to act on the Mac.
- **Vella** sharing Heimdall's speech model, if you use it.

Beta, expect failures:

- **Speaker naming**: tells voices apart and asks who a new one is. New code; labels can be wrong.
- **Wake decision** (no wake word): it sometimes wakes when you were not talking to it, or misses you.
- **Screen recording and memory search**: capture can pause after an archive lock; memory search needs macOS 26.
- **Archive clips**: turns a past window of screen and audio into a movie. Lightly tested.
- **Voice**: English only, one small built-in voice, no instant acknowledgements. Replies held during a call can be
  dropped.
- **Barge-in**: talking over Heimdall stops it. Echo cancellation ships off, so use headphones.
- **Floating cards and plan lists** (islands): the card agent sometimes needs a repair turn.
- **T3 Code delegation**: needs T3 Code 0.0.46 or newer with Orchestrator V2; tested only against nightlies.
- **Agent providers**: Sign in with ChatGPT can fail with HTTP 400 on some models.
- **Settings menu** and **setup wizard**: new, lightly tested on clean machines.
- **Bug notes**: the agent can file notes about its own failures.

Not in this release: in-app updates (off until a release feed exists; `HEIMDALL_UPDATE=1`
turns them on), approval grants, the night pass and a few personal-only parts of the developer's own build.

## Layout

`crates/` holds the Rust workspace (`app` is the binary; see `ARCHITECTURE.md`), `islands/` the floating-card app,
`skills/` the agent's skills, `docs/` the design notes, `scripts/` the build. Each crate has its own README.

## Feedback

Send a bug, complaint or idea to [tobynoskillson.com/feedback](https://tobynoskillson.com/feedback), or have your agent post it:

```sh
curl -d "heimdall: what happened, what you expected, steps to reproduce" https://tobynoskillson.com/api/feedback
curl -F "message=heimdall: the window goes blank" -F file=@shot.png https://tobynoskillson.com/api/feedback   # with a screenshot or log
```

Any text works; JSON or form fields such as `version`, `kind` (`bug`, `feedback`, `idea`), `details` and `contact` (only if you want a reply) are optional. Up to four files of 5 MB each (PNG, JPEG, WebP, GIF, HEIC or plain-text logs). Five reports a minute per address; `curl https://tobynoskillson.com/api/feedback` prints the full usage. [GitHub Issues](https://github.com/TobyNoSkillSon/Heimdall/issues) work too, and fixes are welcome as pull requests.

## License

Apache-2.0, see `LICENSE`. Third-party code and models keep their own licences: `NOTICE`, `THIRD_PARTY_NOTICES.md` and
`licenses/`.
