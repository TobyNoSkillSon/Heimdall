# Setting up Heimdall

The full reference for Heimdall's setup: every step of the Setup window, what each one downloads or changes, what only
the user can click, and `Heimdall setup`, the command a coding agent (Claude Code, Codex, OpenCode…) uses to do the
same end to end. The window and the command share one set of actions and one set of checks: what the command changes
goes through the window's own actions in the running app, and `--check` reads the same readiness list the window's
last step shows.

Code: `crates/ui/src/mac/setup/` (the window), `crates/app/src/setup_wizard.rs` (its actions in the app),
`crates/app/src/pipeline/setup_ready.rs` (the readiness checks), `crates/app/src/setup_cli.rs` (the command). The public edition
is the one described here (docs/editions.md); the personal edition skips the steps marked *public*.

## The Setup window

Setup opens by itself on a first launch, in front, with Heimdall's own menu bar (Heimdall ▸ Quit, Edit) while it
shows; the rest of the time Heimdall lives in the menu bar only. Closing its window before Setup is finished asks "Quit
Heimdall? Setup isn't finished." (Quit, Cancel): there is no half-set-up Heimdall. A relaunch for any reason reopens
Setup at the step it was on. The menu's Set Up… opens it again later.

| # | Step | What it asks | What it does |
|---|---|---|---|
| 1 | Welcome | Set Up, or **Let my agent set it up** | The second shows the one line to paste into a coding agent (below) |
| 2 | What should I call you? | The user's name (optional) | Saved as the user's name; the greeting uses it |
| 3 | Subscription or local model | One tile: Codex, Claude Code, OpenCode Go, This Mac, Custom | The first one connected becomes the agent's model (below) |
| 4 | How should I listen? | The decider's precision: 4-bit (default), 8-bit, 16-bit | Continue starts the model downloads (below) |
| 5 | How should I sound? | One of Phonon S's 25 voices (Edwin default) | The voice; a pick here is kept over a mark's voice |
| 6 | Allow Heimdall | Allow All | macOS asks for the Microphone (required); Screen Recording and Accessibility (optional) need the user in System Settings; System Audio Recording is asked at Allow All too, or when Heimdall first listens |
| 7 | Can you hear me? | The microphone (default: macOS's default input) | Below |
| 8 | Where should I keep my files? | The home folder (default: the user's home) | Heimdall's own files go in `<home>/.heimdall` |
| 9 | Setting up T3 Code | Install (if missing), Connect | T3 Code runs coding tasks; Connect is the user's consent |
| 10 | Let coding agents work without asking? *public* | Set Up, Leave as Is | docs/harness-autonomy.md |
| 11 | Do you want dictation with Vella? | Add Vella, Not now | Homebrew cask `tobynoskillson/tap/vella` |
| 12 | Do you want Arsenal? *public* | Add Arsenal / Use it, Not now | Homebrew cask `tobynoskillson/tap/arsenal`; Use it lets the agent call Arsenal's models (docs/editions.md) |
| 13 | Customise me | A mark; the name (also the wake word) | Each mark brings its name and voice; a typed name or a picked voice is kept |
| 14 | Getting ready *public* | Nothing: a checklist | Start stays off until every row is green (below) |

Screen Recording, Accessibility and System Audio Recording are optional (their rows read Optional until allowed); listening
runs on the microphone alone, and app audio joins when System Audio Recording is allowed, later too, without the
microphone restarting (docs/capture.md, "Microphones first"). Heimdall sees the screen and acts in apps only with
the first two, and none of them ever holds Start back. The microphone is required. When the macOS prompts
close, Setup takes focus back, so its menu bar shows and Return keeps working.

### The agent (step 3)

Setup does not finish without an agent that answered a real request. One of:

- **Codex**: Sign In… opens ChatGPT's sign-in in the browser. The account's default model becomes the agent's.
- **Claude Code**: Add installs T3 Code if it is missing, connects it (the click is the consent), and makes the
  Claude Code relay the agent (docs/claude-relay.md). It needs Claude Code signed in on this Mac.
- **OpenCode Go**: Sign In… opens OpenCode's device sign-in. Claude Haiku 5.5 becomes the agent's model (the catalog's
  first runnable model if it lacks Haiku).
- **This Mac**: the built-in engine (Splash) with Qwen3.8 27B, a 17.4 GB download (UD-Q4_K_XL), shown with its
  progress on Getting ready. It needs about 20 GB of free memory to run.
- **Custom**: an OpenAI- or Anthropic-compatible server: its URL, and a key if it needs one. Its `/models` list must
  answer; its first runnable model becomes the agent's.

The first provider connected becomes the agent while none is chosen; a later connection only adds its models and
never switches the agent (docs/models.md).

### What downloads (step 4)

From the official Hugging Face repositories at pinned revisions, into `<home>/.heimdall/models`, each file checked
against its SHA-256 (docs/models.md, "First-run downloads"). One at a time, resumed after a dropped connection, tried
again by itself after a failure.

| Model | Size | Needed for |
|---|---|---|
| Parakeet | 632 MB | listening |
| Speakers (diarization, voice ID) | 158 MB | listening |
| Decider V6, 4-bit (8-bit 2.10 GB, 16-bit 3.48 GB) | 1.37 GB | listening |
| MossFormer2 | 91 MB | speech split, night pass |
| EmbeddingGemma 2 | 1.53 GB | memory search |

3.78 GB at the default precision; plus 17.4 GB for This Mac's model when chosen.

### The microphone (step 7)

A card with the input Heimdall listens to: macOS's default input until the user picks another, a level meter that
moves only while sound comes in from it, and a pop-up of every connected input. A pick is the menu's own microphone
choice (Settings ▸ Microphone), saved. The meter opens the input only while the step is on screen and the microphone is
allowed, and records nothing. With no input connected the card says "No microphone found"; Getting ready's Microphone
row is then red and Start stays off until one is plugged in or chosen. Listening is on by default; it starts as soon as
its models are in and runs on after Start.

### Getting ready (step 14)

One row each; Start stays off until all are green. A red row says why and offers its fix.

| Row | Green when | Fix offered |
|---|---|---|
| Each model above but the decider | downloaded and its SHA-256 matched | Retry |
| This Mac's model (when chosen) | downloaded and loaded by the engine | Retry |
| Microphone | sound arrived from the chosen input in the last five seconds (it waits for listening; ten seconds of silence turn it red) | Choose… (back to step 7: "No microphone found", "No sound from …"); Retry when the capture didn't start ("Couldn't start listening: …"); "Opening the microphone…" while it starts |
| Voice | Phonon S loaded with the chosen voice | Retry |
| Listening | Parakeet loaded, listening running and the Microphone row green ("Waits for the microphone" until then) | Allow… (the microphone), or wait for the models |
| System audio (only while its ask is out or failed) | never required: "Waits for its permission" or "Off: not allowed", in grey; nothing once its taps run | the menu's or Setup's Allow asks again |
| Memory search (once EmbeddingGemma 2 is in) | its worker loaded and indexed; it needs 8 GB free memory, and without it the row says "Needs 8 GB free memory" in grey and Start doesn't wait for it (`--check`: `optional`) | none |
| Decider | downloaded (with its progress) and its SHA-256 matched, then loaded and answering | Retry (a failed download; a failed load retries by itself) |
| Agent | one real request to the agent's model answered | Choose… (no agent yet), Sign In… (a sign-in failed), Retry |

After Start, the agent greets the user once it is up. The first-run message tells it "The user just finished setting
you up. Greet them and talk to them." and its first reply is a short spoken welcome, a sentence or two, with no tour of
what it can do (the public `onboarding` skill waits until the user asks). Hearing it is the user's sign that Heimdall
started.

## What only the user can do

macOS lets no program grant these; Setup and the command open the right place and wait.

| What | Where |
|---|---|
| Microphone (required) | Click Allow in the macOS prompt (after Allow All, or `--open-permissions`); plug one in if there is none |
| Screen Recording (optional) | System Settings ▸ Privacy & Security ▸ Screen Recording: turn on Heimdall Alpha |
| Accessibility (optional) | System Settings ▸ Privacy & Security ▸ Accessibility: turn on Heimdall Alpha |
| System Audio Recording | Click Allow in the prompt macOS shows after Allow All (or the first time Heimdall listens); unanswered, the microphone can't open either |
| A subscription sign-in | Approve in the browser page Heimdall opens (ChatGPT, OpenCode) |
| Claude Code's own sign-in | `claude` in a terminal, once, if it was never signed in |
| T3 Code's Connect | The Connect click (or `--t3 connect` from the command, which is the user's yes given to their agent) |

## `Heimdall setup`: setup from a coding agent

The command is the app itself: `"/Applications/Heimdall Alpha.app/Contents/MacOS/Heimdall" setup …` (call it `heimdall`
below). Heimdall must be running: the command hands its choices to the running app, which applies them through the
Setup window's own actions, and reads back the window's readiness list. It never prints or stores a secret in a file:
a key comes from an environment variable named by a flag.

What the Welcome page's **Let my agent set it up** gives the user to paste into their agent:

```text
Set up Heimdall for me. Run "/Applications/Heimdall Alpha.app/Contents/MacOS/Heimdall" setup --help and follow it.
```

### Commands

| Command | Does |
|---|---|
| `heimdall setup --help` | This guide for agents, in short: the one question, the commands, the user's clicks |
| `heimdall setup --plan [flags] [--json]` | What would happen (provider, downloads and sizes, the user's clicks); changes nothing |
| `heimdall setup --defaults [flags] [--json]` | Applies the defaults (below), with any flag given taking precedence |
| `heimdall setup [flags] [--json]` | Applies only the flags given |
| `heimdall setup --check [--json]` | The Getting ready checklist, now (the models, Microphone, Voice, Listening, Decider, Agent); exit code below |
| `heimdall setup --wait [--timeout SECONDS] [--json]` | `--check` repeated until all green, a row needs the user, or the timeout (default 1800) |
| `heimdall setup --wizard` | Opens the Setup window |

### Flags

| Flag | Values | Maps to |
|---|---|---|
| `--provider` | `claude-code` · `codex` · `opencode-go` · `local` · `custom` | Step 3's tile |
| `--custom-url URL` | the server's base URL | Custom's URL |
| `--custom-kind` | `openai` (default) · `anthropic` | which API the server speaks |
| `--custom-key-env VAR` | the name of an environment variable holding the key | Custom's key (omit for a keyless server) |
| `--decider-precision` | `4` (default) · `8` · `16` | Step 4 |
| `--voice NAME` | one of Phonon S's voices | Step 5 |
| `--user-name TEXT` | the user's name | Step 2 |
| `--name TEXT` | the assistant's name and wake word (default Heimdall) | Step 13 |
| `--mark NAME` | `helmet` · `sparkle` · `leaf` · `moon.stars` · `bird` · `cat` · `hare` · `flame` · `bolt` · `crown` · `eye` · `atom` | Step 13 (brings its name and voice unless given) |
| `--home PATH` | an existing folder | Step 8 (default: keep the current one) |
| `--t3` | `connect` (default with `claude-code`) · `skip` | Step 9 |
| `--harnesses` | `set` · `leave` (default) | Step 10 |
| `--vella`, `--arsenal` | `add` · `skip` (default) | Steps 11, 12 |
| `--open-permissions` | | Step 6's Allow All: the macOS prompts appear for the user |
| `--finish` | | Start, once `--check` is all green (the greeting follows) |
| `--json` | | One JSON object per line (below) |

### Defaults (`--defaults`)

- Provider: `claude-code` when the command runs inside Claude Code (`CLAUDECODE=1`); `codex` inside Codex
  (`CODEX_HOME` or `CODEX_SANDBOX` set); else none, and the command exits 2 asking the one question.
- Everything else: the window's suggestions (decider 4-bit, Edwin, the name Heimdall, the current home, T3 Code
  connected for `claude-code`, Vella and Arsenal skipped).

### The one question

When the provider can't be decided, ask the user exactly one thing: "Which should run Heimdall's agent: your Claude
Code, Codex or OpenCode Go subscription, this Mac (a 17.4 GB model), or a server of your own?" Then run the command
again with `--provider`.

### Output and exit codes

`--json` prints one object per line: `{"step": "...", "state": "done" | "working" | "needs_user" | "failed",
"detail": "..."}`, the readiness rows last with `"step": "ready:<row>"`. Human text otherwise.

| Code | Meaning |
|---|---|
| 0 | Everything green (`--check`, `--wait`), or the choices were handed to the app |
| 2 | Needs the user: a click, a sign-in, or the one question; `detail` says which |
| 4 | A provider or key failed |
| 1 | Anything else (Heimdall not running, a bad flag) |

### An agent's run

```sh
heimdall setup --plan --defaults --json
heimdall setup --defaults --open-permissions --json   # exit 2: tell the user each click in one sentence
heimdall setup --wait --json                          # until green or a row needs the user
heimdall setup --finish
```

### MCP

There are no setup tools on Heimdall's MCP server yet: an agent uses the command. (Heimdall's own MCP tools belong to
its agent, docs/mcp.md.)

### How it reaches the app

The command writes one request file into `<home>/.heimdall/setup/requests/`; the running app takes it within two
seconds and applies each choice as the Setup window's matching action (`setup_cli::Request`). The app keeps
`<home>/.heimdall/setup/status.json` current with the readiness rows, which `--check` reads. Without a running app the
command exits 1.
