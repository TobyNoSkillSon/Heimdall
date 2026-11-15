# Contributing to Heimdall

How to build and test a change, where new code and experiments go, and the rules every doc page follows.

Read this when: you want to change Heimdall's code, run its tests, add a crate or a script, or write or change a doc
page.
Code: `scripts/check.sh`, `scripts/affected_crates.py`, `scripts/model_tests.py`, `.config/nextest.toml`,
`Cargo.toml`.

## What you need

- An Apple Silicon Mac with macOS 14.4 or later.
- Xcode, selected with `xcode-select` (not the Command Line Tools alone). Why: the speakers compiler lock
  (`crates/speakers/native/build-lock.json`) names Xcode's clang, and the Metal compiler (`xcrun metal`) is Xcode's.
- Rust in `$HOME/.cargo/bin`.
- `cargo-nextest`. Install the prebuilt binary:
  `curl -LsSf https://get.nexte.st/latest/mac | tar zxf - -C "$HOME/.cargo/bin"`.

Building a signed app also needs a signing identity and the model assets: [install.md](install.md).

## Check a change

Use the one script. Never run a hand-made `cargo nextest` over the workspace or point `CARGO_TARGET_DIR` at a fresh
folder. Why: a hand-made run skips the RAM disk and rebuilds everything from nothing, so the suite takes several times
as long and tests time out.

1. While you work, run the tests of the crate you touch: `cargo test -p <crate>` (crate names:
   [architecture.md](architecture.md#crate-map)).
2. Before you hand the change on, run `scripts/check.sh --change` (compares with `main`; pass another base as the
   second argument). It runs the tests and doctests of every crate the change can reach and the script tests.
3. After a large change, run `scripts/check.sh --tests`: every workspace test and doctest, the script tests and the
   public-edition gates.
4. Read the last line: `CHECK PASSED`, or `CHECK FAILED` after the last 30 lines of each failed step's log.

The modes (`scripts/check.sh`; an unknown flag exits 2 with the usage line):

| Mode | Runs | Needs a clean tree |
|---|---|---|
| `--change [BASE]` | tests and doctests of the changed crates, their dependents and crates that read their files (`scripts/affected_crates.py`), plus the script tests. When `app`, `agent` or `listen` is reached, also the public-edition gate tests. A change outside `crates/` (manifests, `Cargo.lock`, `.config/nextest.toml`, scripts, skills, docs) runs everything. | no |
| `--tests` | every workspace test and doctest, the script tests, and the public-edition gate tests (built with `-p app -p agent -p listen --no-default-features --features app/public`) | no |
| (none, full) | everything `--tests` runs, plus the signed personal candidate (`scripts/bundle.sh --edition personal` into `target/release-candidate`) and its verify, then the model tests one at a time (`scripts/model_tests.py`) | yes: sources committed |
| `--status` | prints the run in progress in this worktree, or the last run's result | — |

The full mode builds the personal edition, which needs assets and a signing identity a public checkout does not have.
Contributors use `--change` and `--tests`.

What the script does for you:

- Every step runs, even after another fails. Nothing installs, launches the installed app or requests permissions.
- One run per worktree at a time (a `lockf` lock in the git directory). A second run stops at once and names the
  running one and its log.
- Each run writes `target/check/<UTC time>-<mode>/` (`target/check/latest` points at the newest): one log per step,
  `times`, `failed`, `result` (mode, commit, whether the tree was dirty) and a JUnit report per nextest run
  (`<step>.junit.xml`). The ten newest runs are kept.
- `target/test` and `target/release-candidate` are the build cache. Keep them.
- Every step runs at `nice 19` with two build jobs in all; on power with the Mac idle for 10 minutes, builds use
  every core. `HEIMDALL_JOBS` overrides the job count.
- Test temp files go to a private RAM disk the script creates and ejects on exit. Why: the archive journal commits
  with `F_FULLFSYNC`, which is slow on the SSD when many test processes flush at once. Code and SQLite durability
  settings are the same as in production.
- Tests run under `cargo-nextest`, one process per test. `[profile.test]` in `Cargo.toml` uses `opt-level = 1`, no
  debug info, and keeps debug assertions and overflow checks on.

## Tests

- Every fixed bug leaves a test behind.
- The default suite uses no model inference. A test that loads a model is marked `#[ignore = "model: <what it loads>"]`; the
  full check runs these one at a time through `scripts/model_tests.py`.
- A test never requests a permission, opens a microphone, starts a tap, records the screen or touches the user's
  real data folder. Tests that need the live app are `#[ignore]` and documented on their topic page (for example
  [capture.md](capture.md)).
- UI is checked by offscreen renders at real size: `heimdall-ui-demo` ([crates/ui/README.md](../crates/ui/README.md))
  and the islands lab flags ([viewer.md](viewer.md#check-it), [islands/README.md](../islands/README.md)).

## Where code goes

- `crates/<domain>/`: shipping code, one crate per domain where the boundary is clean
  ([architecture.md](architecture.md#crate-map)). Keep parts independent so they can change in parallel.
- `lab/` beside the code: experiments, renders, measurement scripts and evidence. Nothing in `lab/` is built or
  shipped.
- `scripts/`: build, check and release scripts, each with a test in `scripts/test_*.py` where it has logic.
- `skills/`: skills bundled into the app ([skills.md](skills.md)). Prompt text lives in `crates/agent/prompt/`.
- A feature only one edition has sits behind the `public` Cargo feature ([editions.md](editions.md)).
- A new tool for the agent: [mcp.md](mcp.md#add-a-tool). A new background job:
  [background-call.md](background-call.md). A new setting: [settings.md](settings.md).

## Change the docs

Docs ship inside the app (the agent reads them through `HEIMDALL_DOCS_DIR`), so a doc change is a code change:
commit it with the change it describes.

- One page per topic in `docs/`. Each fact lives on one page; other pages link to it.
- Page shape, in this order: `# Title`; one or two sentences on what it is for; `Read this when:` with the tasks that
  bring a reader here; `Code:` with the main paths; sections named by task or thing; `## See also` with links.
- Procedures are numbered steps with the exact command or file, ending with how to check it worked. Reference is a
  table or bullets, one row per item.
- Use a Mermaid diagram where a pipeline, a state machine or who-calls-whom is easier seen than told. Quote labels
  that hold punctuation.
- Exact names, paths, commands, settings keys, defaults and limits go in backticks. A number names the constant that
  holds it; a measured number says what it was measured on.
- Present tense, short sentences. No history, no dates of decisions, no quotes. "Why" is one line, only where it
  stops a wrong change. The git log holds history.
- Where the doc and the code disagree, the code wins: fix the doc, or fix the code and the doc together.
- Links are relative Markdown links that resolve on GitHub from the page's own folder. Code paths that are not links
  go in backticks from the repository root.
- A crate's `README.md` is its code map: what the crate is, its main files and entry points, its tests and how to run
  them, and links to the topic pages for behaviour.
- Some pages are read by the running app: `docs/connectors.md` (pinned by `crates/agent/tests/prompt.rs`) and
  `docs/emergency.md` (shipped as `Resources/emergency.md`, read by the relay). Keep their instructions' meaning, and
  run `cargo test -p agent --test prompt` after changing `connectors.md`.

## See also

- [architecture.md](architecture.md): the crates and how a turn flows through them
- [install.md](install.md): build a signed app, install it, roll back
- [editions.md](editions.md): what differs between the public and personal editions
- [README.md](README.md): every page, by task
