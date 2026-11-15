# Set up Heimdall

How a new user gets from a fresh install to a first spoken answer: installing the app, every step of the Setup window,
what each step downloads or changes, what only the user can click, and `Heimdall setup`, the command a coding agent
(Claude Code, Codex, OpenCode) runs to do the same end to end. The window and the command share one set of actions and
one set of checks.

Read this when: you install Heimdall for the first time, you help a user through Setup, an agent must set Heimdall up
from a terminal, a Getting ready row stays red, agents' commands run slowly, or you change a Setup step.
Code: `crates/ui/src/mac/setup/` (the window), `crates/app/src/setup_wizard.rs` (its actions in the app),
`crates/app/src/pipeline/setup_ready.rs` (Getting ready's checks), `crates/app/src/pipeline/setup_phrase.rs` (the
spoken last step), `crates/app/src/setup_cli.rs` (the command).

This page describes the public edition, Heimdall Alpha. The personal edition's Setup has fewer steps
([editions.md](editions.md)).

## Before you start

- A Mac with Apple silicon on macOS 14.4 or later (`LSMinimumSystemVersion` in `scripts/bundle.sh`).
- About 5 GB of free disk and a network connection for the first-run models (below), plus 17.4 GB more if the agent
  runs on this Mac.
- A microphone. Setup does not finish without one.
- One way to run the agent: a Claude Code, Codex (ChatGPT) or OpenCode Go subscription, an OpenAI- or
  Anthropic-compatible server, or about 20 GB of free memory for the built-in engine.

## Install and first run

1. Get the app. Either install the Homebrew cask (`brew install --cask tobynoskillson/tap/heimdall-alpha`; the cask
   is `packaging/homebrew/heimdall-alpha.rb`), or download
   `Heimdall-Alpha-<version>-arm64.zip` from the repository's GitHub releases, unzip it and move `Heimdall Alpha.app`
   to `/Applications`. To build it from source instead (Xcode with its Metal Toolchain, Rust, CMake, Python 3.11+ and
   ffmpeg), follow [install.md](install.md#before-the-first-build), then continue at step 3; a local build carries no
   download flag.
2. The app is ad-hoc signed, not notarised. The cask clears the download flag itself; after a manual download, clear
   it before the first launch:

   ```sh
   xattr -dr com.apple.quarantine "/Applications/Heimdall Alpha.app"
   ```

3. Open Heimdall Alpha. Setup opens in front, with its own menu bar (Heimdall ▸ Quit, Edit) while it shows.
4. Go through the steps (next section) with Continue, or click **Let my agent set it up** on Welcome and paste the line
   it shows into a coding agent ([`Heimdall setup`](#heimdall-setup-setup-from-a-coding-agent)).
5. Wait on Getting ready until every required row is green.
6. Say "Heimdall, show me what you can do" (with the assistant's name if you changed it). Setup closes.
7. Check it worked: the agent answers out loud with a short demo, and Heimdall's mark stays in the menu bar. Heimdall
   lives in the menu bar only from then on.

```mermaid
flowchart LR
  A[Welcome] --> B[Name] --> C[Agent] --> D[Listening] --> E[Voice] --> F[Allow]
  F --> G[Microphone] --> H[Home folder] --> I[T3 Code] --> J[Coding agents]
  J --> K[Vella] --> L[Arsenal] --> M[Customise] --> N[Getting ready] --> O["Say the phrase"]
```

## The steps

| # | Title | What it asks | What it does |
|---|---|---|---|
| 1 | Welcome to Heimdall | Set Up, or **Let my agent set it up** | The second shows the one line to paste into a coding agent, with Copy |
| 2 | What should I call you? | The user's name (optional) | Saved as the user's name; the greeting uses it |
| 3 | Do you have a subscription, or do you want a local model? | One tile: Codex, Claude Code, OpenCode Go, This Mac, Custom | Connects the agent's model ([Choose the agent](#choose-the-agent)) |
| 4 | How should I listen? | The decider's precision: 4-bit (default), 8-bit, 16-bit | Continue starts the model downloads ([What downloads](#what-downloads)) |
| 5 | How should I sound? | One of Phonon S's 25 voices (Edwin by default) | The voice; a voice picked here is kept over a mark's voice ([voices.md](voices.md)) |
| 6 | Allow Heimdall | Allow All | macOS asks for the Microphone (required) and System Audio Recording (optional); Screen Recording and Accessibility (optional) need the user in System Settings |
| 7 | Can you hear me? | The microphone (default: macOS's default input) | [The microphone](#the-microphone) |
| 8 | Where should I keep my files? | The home folder (default: the user's home folder) | Heimdall's own files go in `<home>/.heimdall` ([home.md](home.md)) |
| 9 | Setting up T3 Code | Install (when missing), Connect | T3 Code runs coding tasks; Connect is the user's consent ([t3-orchestration.md](t3-orchestration.md)) |
| 10 | Let coding agents work without asking? | Set Up, Leave as Is | [harness-autonomy.md](harness-autonomy.md) |
| 11 | Do you want dictation with Vella? | Add Vella, Not now | Installs the Homebrew cask `tobynoskillson/tap/vella`; without Homebrew, opens Vella's page ([vella.md](vella.md)) |
| 12 | Do you want Arsenal? | Add Arsenal (Use it when installed), Not now | Installs the Homebrew cask `tobynoskillson/tap/arsenal` (without Homebrew, opens its page); Use it gives the agent Arsenal's models as tools ([connectors.md](connectors.md)) |
| 13 | Customise me | A mark; the name, the name it answers to | Each mark brings its own name and voice; a typed name or a voice picked in step 5 is kept ([personalise.md](personalise.md)) |
| 14 | Getting ready | Nothing to click | A checklist; once all green, the user says the phrase ([Getting ready](#getting-ready)) |

- The microphone is the only required permission. Listening runs on the microphone alone; app audio joins when System
  Audio Recording is allowed, at any later time, without restarting the microphone ([capture.md](capture.md)).
  Heimdall sees the screen and acts in apps only with Screen Recording and Accessibility. No optional permission holds
  Setup back.
- When the macOS prompts close, Setup takes focus back, so its menu bar shows and Return keeps working.

## Choose the agent

Setup does not finish until the chosen agent has answered one real request. The first provider connected becomes the
agent; a later connection adds its models and never switches the agent ([models.md](models.md)).

- **Codex**: Sign In… opens ChatGPT's sign-in in the browser ([chatgpt.md](chatgpt.md)).
- **Claude Code**: Add installs T3 Code if it is missing, connects it (the click is the consent) and makes the Claude
  Code relay the agent ([claude-relay.md](claude-relay.md)). Claude Code must be signed in on this Mac.
- **OpenCode Go**: Sign In… opens OpenCode's device sign-in in the browser.
- **This Mac**: the built-in engine (Splash) with Qwen3.8 27B, a 17.4 GB download (UD-Q4_K_XL), shown with its
  progress on Getting ready. It needs about 20 GB of free memory ([inference-engine.md](inference-engine.md)).
- **Custom**: an OpenAI- or Anthropic-compatible server: its URL, and a key if it needs one. Its `/models` list must
  answer; its first runnable model becomes the agent's.

## What downloads

Continue on step 4 starts the downloads (`model_store::first_run_groups`). They come from the official Hugging Face
repositories at pinned revisions (`crates/app/src/model_downloads.lock.json`), go into `<home>/.heimdall/models`, and
each file is checked against its SHA-256. One at a time, resumed after a dropped connection, retried by itself after a
failure.

Parakeet, the speaker models, the decider, MossFormer2, EmbeddingGemma 2 and the reranker: 4.98 GB at the default
precision,
plus 17.4 GB for This Mac's model when it is chosen. Each model, its size and its pin:
[models.md](models.md#first-run-downloads-public-edition).

## The microphone

- The card shows the input Heimdall listens to (macOS's default input until the user picks another), a level meter,
  and a pop-up of every connected input.
- A pick is the menu's own microphone choice, saved ([settings.md](settings.md)).
- The meter opens the input only while this step is on screen and the microphone is allowed. It records nothing.
- With no input connected the card says "No microphone found"; Getting ready's Microphone row is then red until one
  is plugged in or chosen.
- Listening is on by default. It starts as soon as its models are in and keeps running after Setup.

## Getting ready

One row each. The phrase card appears only when every model download has finished and every row is green; an
optional row never holds it back. A red row says why and offers its fix. Once every model is in, their rows become one
"Models" row with their total size, so the card fits under the list at the window's default size; when the card
arrives or moves (more rows above it), the page scrolls it into view.

| Row | Green when | Otherwise |
|---|---|---|
| Parakeet, Speakers, MossFormer2, EmbeddingGemma 2, Memory ranking (the reranker; never its model's name) | downloaded and its SHA-256 matched | progress; Retry after a failure |
| Microphone | sound arrived from the chosen input in the last 5 s | "Waits for listening", "Opening the microphone…", "Listening for sound…"; red after `MIC_SILENT` (10 s) of silence: "No sound from …" or "No microphone found" with Choose… (back to step 7); "Couldn't start listening: …" with Retry; "Microphone access is off in System Settings" with Allow… while macOS denies it (`setup_ready::microphone_access`) |
| Voice | Phonon S loaded with the chosen voice and Heimdall's own voice learnt from it | "Speaking is off" with Turn On; "Couldn't learn the voice yet" with Retry |
| Listening | Parakeet loaded, listening running and the Microphone row green | "Waits for the microphone", "Waits for its models"; "Allow Heimdall first" with Allow…; "Listening is off" with Turn On |
| Decider | downloaded, its SHA-256 matched, loaded and answering | its download progress, Retry after a failed download; a failed load retries by itself |
| Memory search (once EmbeddingGemma 2 is in) | its worker loaded and indexed | optional, in grey: "Needs 6 GB free memory" (`MEMORY_NEEDED`, the worker's `admissionBytes`) or "Starts with the first thing to index" |
| System audio (only while its ask is out, failed or denied) | never required | optional, in grey: "Waits for its permission", or "Off: not allowed" with Allow…; no row once its taps run |
| This Mac (only when the built-in engine is the agent) | downloaded and loaded by the engine | progress; Download, or Retry after a failure |
| Agent | one real request to the agent's model answered, through the agent's own route, and its key or T3 pairing reads from the Keychain | "No agent chosen yet" with Choose…; "T3 Code isn't connected yet" with Connect…; a refused sign-in or key with Sign In…; a key this build can't read ("This build can't read the saved key: re-enter it in Providers", or "Keychain locked: …") with Re-enter Key… (`setup_ready::agent_key_problem`); anything else with Retry |

## Say the phrase to finish

Getting ready cannot be clicked through. Once every row is green, a card shows the phrase "Heimdall, show me what you
can do" (with the assistant's name; always English) and five level bars that move only while sound comes in. The user
says it, and Setup closes when Heimdall hears it.

- The match ignores case and punctuation and accepts it with or without a wake name in front, since speech
  recognition may hear "Heimdall" as "Hindor" or "Hindol" (`setup_phrase::heard`). Close variants count ("could",
  "show us").
- The words become the agent's first message (`setup_wizard::SAID` and `DEMO`): the agent answers with a short spoken
  demo, not a tour (the public `onboarding` skill's "First run").
- The audio becomes the user's voice profile (`user-voice.json` in the data folder, `UserVoice::enrol`). Until a
  profile has the user's voice, only the wake name wakes the agent, so background talk cannot
  ([decision-chain.md](decision-chain.md), [speakers.md](speakers.md)).
- After `HINT_AFTER` (8 s) of silence the card shows "Say it a little louder" and keeps listening.
- Only when the microphone has truly failed (no input, a capture that did not start) does the card show why, its fix,
  and **Start without speaking**. That ends Setup with a plain greeting instead (`setup_wizard::GREETING`).

## What only the user can do

macOS lets no program grant these. Setup and the command open the right place and wait.

| What | Where |
|---|---|
| Microphone (required) | Click Allow in the macOS prompt (after Allow All, or `--open-permissions`); plug one in if there is none |
| Screen Recording (optional) | System Settings ▸ Privacy & Security ▸ Screen Recording: turn on Heimdall Alpha |
| Accessibility (optional) | System Settings ▸ Privacy & Security ▸ Accessibility: turn on Heimdall Alpha |
| System Audio Recording (optional) | Click Allow in the prompt after Allow All; unanswered or denied, Getting ready's System audio row offers Allow… |
| A subscription sign-in | Approve in the browser page Heimdall opens (ChatGPT, OpenCode) |
| Claude Code's own sign-in | Run `claude` in a terminal once, if it was never signed in |
| T3 Code's Connect | The Connect click, or `--t3 connect` given to the user's own agent |
| The phrase | Say it on Getting ready |
| Developer Tools (optional, faster agents) | System Settings ▸ Privacy & Security ▸ Developer Tools: turn on T3 Code and Heimdall Alpha ([below](#faster-agent-commands-developer-tools)) |

## Faster agent commands: Developer Tools

macOS checks a freshly built binary or a newly written script the first time it runs: `syspolicyd` runs a
Gatekeeper and XProtect assessment before it starts. An app listed and switched on
in **System Settings ▸ Privacy & Security ▸ Developer Tools** ("run software locally that does not meet the system's
security policy") is exempt, and so is every process it starts, down the whole tree: T3 Code → Claude Code or Codex
→ shell → build → test binary. Coding agents build and write new programs all day, so the check lands on their tool
calls.

Measured on an M5 Max, macOS 27.0.1, 14 Nov 2026, 10 to 20 runs each: a freshly built binary's first run took a median
300 to 720 ms outside the exemption and 4 to 6 ms inside it; a newly written script 370 ms against 9 ms. Commands that
run an existing program (`ls`, `git`, `zsh -c …`) cost the same 3 to 5 ms either way. On the same Mac, one morning of
agent work ran 322 of these checks in two and a half hours, a median 240 ms each, the slowest 18 s.

- **The trade-off.** Malware an agent downloads, builds or writes and then runs from an exempt app is not checked by
  XProtect at launch. It grants no file, screen or network access, and Eclectic Light's tests on macOS 26 found that
  quarantined downloads are still blocked (not checked here). It is optional: everything works without it, only slower.
- **Which apps.** T3 Code, which starts every coding task, and Heimdall Alpha, whose agent runs shells and scripts. A
  terminal the user codes in can go on too. A process started outside the app's tree (a `tmux` server, a launch
  agent) needs its own entry.
- **Updates keep it.** The entry follows the app's bundle ID and signing identity: an update signed by the same
  developer keeps it (T3 Code's builds, and Heimdall Alpha's, which `scripts/bundle.sh` signs with one fixed
  identity). An ad-hoc signed app loses it on every update, as nextest's macOS guide reports for Homebrew's `tmux`.
- **Relaunch after switching it on.** `syspolicyd` asks once per running app and remembers the answer, so the
  exemption reaches an app's processes after that app is quit and reopened.

### How the agent sets it up

1. Open the pane for the user; they never have to look for it:

   ```sh
   open "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_DevTools"
   ```

   Checked on macOS 27. If that opens Privacy & Security without the Developer Tools row, the pane is hidden on that
   Mac: `sudo spctl developer-mode enable-terminal` adds it (and Terminal), which needs the user's password, so the
   user runs it or approves it.
2. Tell the user, in one sentence: turn on T3 Code and Heimdall Alpha, using **+** to add one that is not listed, then
   quit and reopen each.
3. Check from the app's own tree. A T3 task's result covers T3 Code, the agent's own covers Heimdall Alpha; in each, time a script that has never
   run:

   ```sh
   d=$(mktemp -d); printf '#!/bin/sh\n# %s\n' "$$-$RANDOM" > "$d/c.sh"; chmod +x "$d/c.sh"; /usr/bin/time -p "$d/c.sh"
   ```

   `real` under `0.05` means exempt; `0.15` or more means the check still runs: the app is off in the list or was not
   reopened. Run it twice if the first answer is in between.

## `Heimdall setup`: setup from a coding agent

The command is the app binary itself: `"/Applications/Heimdall Alpha.app/Contents/MacOS/Heimdall" setup …` (called
`heimdall` below). Heimdall must be running: the command writes the choices into a request the running app applies
through the Setup window's own actions, and reads back Getting ready's rows. It never prints or stores a secret: a key
comes from an environment variable named by a flag.

What **Let my agent set it up** gives the user to paste (`setup::agent_line`, with the running app's path):

```text
Set up Heimdall for me. Run "/Applications/Heimdall Alpha.app/Contents/MacOS/Heimdall" setup --help and follow it.
```

### Commands

| Command | Does |
|---|---|
| `heimdall setup --help` | A short guide for agents: the one question, the commands, the user's clicks |
| `heimdall setup --plan [flags] [--json]` | What would happen (the agent, downloads and sizes, the user's clicks); changes nothing |
| `heimdall setup --defaults [flags] [--json]` | Applies the defaults (below); any flag given takes precedence |
| `heimdall setup [flags] [--json]` | Applies only the flags given |
| `heimdall setup --check [--json]` | Getting ready's rows now |
| `heimdall setup --wait [--timeout SECONDS] [--json]` | `--check` every 5 s until all green, a row needs the user, the agent fails, or the timeout (default 1800) |
| `heimdall setup --finish [--timeout SECONDS]` | Once every row but the phrase is green, waits for the user to say the phrase |
| `heimdall setup --wizard` | Opens the Setup window |

### Flags

| Flag | Values | Step |
|---|---|---|
| `--provider` | `claude-code`, `codex`, `opencode-go`, `local`, `custom` | 3 |
| `--custom-url URL` | the server's base URL (required with `custom`) | 3 |
| `--custom-kind` | `openai` (default), `anthropic` | 3 |
| `--custom-key-env VAR` | the name of an environment variable holding the key; omit for a keyless server | 3 |
| `--decider-precision` | `4` (default), `8`, `16` | 4 |
| `--voice NAME` | one of Phonon S's voices | 5 |
| `--open-permissions` | | 6: Allow All; the macOS prompts appear for the user |
| `--home PATH` | an existing folder (default: keep the current one); applied last, as the move relaunches Heimdall | 8 |
| `--t3` | `connect` (default with `claude-code`), `skip` | 9 |
| `--harnesses` | `set`, `leave` (default) | 10 |
| `--vella`, `--arsenal` | `add`, `skip` (default) | 11, 12 |
| `--user-name TEXT` | the user's name | 2 |
| `--mark NAME` | `helmet`, `sparkle`, `leaf`, `moon.stars`, `bird`, `cat`, `hare`, `flame`, `bolt`, `crown`, `eye`, `atom` (`setup_cli::MARKS`); brings its name and voice unless `--name` or `--voice` is given | 13 |
| `--name TEXT` | the assistant's name, the name it answers to (default Heimdall) | 13 |
| `--json` | | one JSON object per line |

### Defaults

- Provider: `claude-code` when the command runs inside Claude Code (`CLAUDECODE=1`), `codex` inside Codex
  (`CODEX_HOME` or `CODEX_SANDBOX` set), otherwise none: the command exits 2 with the one question.
- Everything else is the window's suggestion: the 4-bit decider, Edwin, the name Heimdall, the current home folder,
  T3 Code connected for `claude-code`, Vella and Arsenal skipped, the coding agents left as they are.

### The one question

When the provider can't be decided, ask the user exactly this (`setup_cli::QUESTION`): "Which should run Heimdall's
agent: your Claude Code, Codex or OpenCode Go subscription, this Mac (a 17.4 GB model), or a server of your own?" Then
run the command again with `--provider`.

### Output and exit codes

`--json` prints one object per line: `{"step": "...", "state": "...", "detail": "..."}`. Applied choices read
`done`, `--plan` lines `planned`, and anything waiting on the user `needs_user`. Getting ready's rows come last as
`"step": "ready:<row>"` with `ready`, `working`, `needs_user`, `failed` or `optional`. Without `--json` the lines are
plain text.

| Code | Meaning |
|---|---|
| 0 | Every row green (`--check`, `--wait`), the user said the phrase (`--finish`), or the choices were handed to the app |
| 2 | Needs the user: a click, a sign-in, the phrase, or the one question; `detail` says which |
| 4 | The agent failed: its first request, a custom server that did not answer, or an unset key variable |
| 1 | Anything else: Heimdall not running, a bad flag, rows still working when `--check` or the timeout ran |

### An agent's run

1. Show the plan:

   ```sh
   heimdall setup --plan --defaults --json
   ```

2. Apply it and open the permission prompts. Exit 2 lists what the user must click; tell them, one sentence each.

   ```sh
   heimdall setup --defaults --open-permissions --json
   ```

3. Wait until green or until a row needs the user:

   ```sh
   heimdall setup --wait --json
   ```

4. When only the `phrase` row is left (exit 2), tell the user to say "Heimdall, show me what you can do", then wait for
   it:

   ```sh
   heimdall setup --finish
   ```

5. Check it worked: `--finish` exits 0 with "the user said it: Setup is finished and the agent answers".

Heimdall's MCP server has no setup tools; an agent uses the command ([mcp.md](mcp.md)).

### How it reaches the app

- The command writes one request file into `<data>/setup/requests/`. The running app takes it on its next settings
  tick (every 2 s) and applies each choice as the Setup window's matching action (`setup_cli::actions_for`).
- The app rewrites `<data>/setup/status.json` with Getting ready's rows every `HEARTBEAT` (10 s), and `--check` reads
  it. A status older than `STALE` (30 s) means Heimdall is not running, and the command exits 1.
- `--finish` is applied only if Getting ready is all green at that moment in the app, never on the status file's word.

`<data>` is Heimdall's data folder, normally `<home>/.heimdall` ([home.md](home.md)).

## After an update

macOS ties the microphone, Screen Recording and Accessibility grants, and access to the provider keys Heimdall saved in
the login Keychain, to the app's signature. An ad-hoc signed build (`scripts/bundle.sh --adhoc`, the alpha releases)
has a new signature every build, so an update can lose them. At every launch of the installed app after Setup's Allow
All (`crates/app/src/launch_permissions.rs`):

- Microphone not decided for this build (also after `tccutil reset Microphone`), while Listening is on: macOS's
  prompt appears; the user clicks Allow.
- Microphone denied, while Listening is on: a red pill "Microphone access lost". Clicking it opens System Settings ▸
  Privacy & Security ▸ Microphone; turn Heimdall Alpha on. The pill goes when the grant is back.
- Screen Recording (while Seeing is on) or Accessibility, once held by this app and gone now: the same pill for that
  pane, and Screen Recording is asked for again. Never held: nothing, since both are optional.
- A provider key or T3 pairing the earlier build saved and this one can't read: a red pill "Saved key unreadable ·
  <provider>" and, while Setup is open, the Agent row's Re-enter Key…. Unlocking the Keychain does not help; entering
  the key again in Providers saves it for this build. A locked Keychain says "Keychain locked" and asks to unlock it
  first ([crates/app/src/keychain_notice.rs](../crates/app/src/keychain_notice.rs)).

## When Setup is closed or stops

- Closing the window before Setup is finished asks "Quit Heimdall? Setup isn't finished." (Quit, Cancel). There is no
  half-set-up Heimdall.
- A relaunch for any reason reopens Setup at the step it was on (`setup_wizard::resume_step`). A relaunch past step 4
  starts any download that had not begun.
- Later, `heimdall setup --wizard` opens the window again. The menu's Home Folder… opens it at the home step, and the
  menu's status line offers Set Up… when a permission was taken away.

## See also

- [harness-autonomy.md](harness-autonomy.md): what step 10's Set Up changes in each coding agent
- [home.md](home.md): the home folder, the data folder and moving them
- [models.md](models.md): providers, model choice and first-run downloads
- [chatgpt.md](chatgpt.md): the Codex (ChatGPT) sign-in
- [claude-relay.md](claude-relay.md): the agent as a Claude Code thread through T3
- [install.md](install.md): building and installing from source
- [resilience.md](resilience.md): when something fails after Setup
