# Agent models and on-device models

Which models Heimdall's agent can run, how large one request may get, when history compacts, how thinking and speed
are set, which model background jobs use, and which on-device models the public edition downloads on first run.

Read this when: you want to add or change a provider, change a model's context budget or compaction thresholds,
change thinking levels or Fast mode, change the background model, change the first-run downloads, or point downloads
at a test mirror.
Code: `crates/agent/src/provider.rs` (`PROVIDERS`, `ApiModel`, `Limits`, `AgentModel`), `crates/agent/src/runtime.rs`
(`configure`, `configure_api_carrying`), `crates/app/src/model_store.rs`, `crates/app/src/model_downloads.lock.json`.

Connect flows live elsewhere: ChatGPT in [chatgpt.md](chatgpt.md), the other providers in
[crates/agent/README.md](../crates/agent/README.md). Every settings key: [settings.md](settings.md).

## Providers

`provider::PROVIDERS`, in menu order (subscriptions first, then API keys, then custom servers):

| Id | Name | Credential | Notes |
|---|---|---|---|
| `openai` | ChatGPT | Sign in with ChatGPT | Subscription. Models from the account's own list. |
| `opencode-go` | OpenCode Go | Console sign-in or a key | Subscription. Models from `/v1/models`; models.dev fills what Pi's catalog lacks. |
| `anthropic` | Anthropic | API key (and a workspace for `sk-ant-usr-…` keys) | One-hour prompt cache (`hour_cache`). |
| `openai-compatible` | OpenAI-Compatible | Base URL, key optional | Any server speaking OpenAI chat completions; models from `<base>/models`. |
| `anthropic-compatible` | Anthropic-Compatible | Base URL without `/v1`, key optional | Any server speaking the Anthropic Messages API; models from `<base>/v1/models`. |

The built-in engine ([inference-engine.md](inference-engine.md)) is not a provider.

- Retired ids: `own-engine` models move to `openai-compatible` with their URL, once
  (`Settings::migrate_retired_providers`, `provider::adopt_retired_endpoint`); `openrouter` models are dropped with a
  log line, and OpenRouter is added again as a custom provider.
- Keys live in the login Keychain, never in settings, logs or Pi's files. Item names: "Heimdall Alpha API provider:
  <name>" in the public edition, "Heimdall API provider: <name>" in the personal one.
- Each provider is its own billing route. Never mix a subscription with a metered API and never fall back from one to
  another.
- Start state: the public edition starts with no agent model (`AgentModel::Unchosen`) and no added rows. The personal
  edition starts on Claude Haiku 5.5 on OpenCode Go, with Opus, Sonnet and Haiku 5.5 on Anthropic added
  (`settings::default_api_models`).
- A connection never switches an agent that already has a model. While none is chosen, the first provider connected
  becomes the agent with its `connect_default`: ChatGPT `chatgpt::DEFAULT_MODEL` (GPT-6.1 Sol), OpenCode Go Claude
  Haiku 5.5, Anthropic Claude Opus 5.5; a custom server offers its catalog's first runnable model. The hub repeats the
  offer when the catalog loads (`Hub::offer_default` in `crates/app/src/api_providers.rs`). Setup's Claude Code tile
  makes the relay the agent instead ([claude-relay.md](claude-relay.md)).
- One model runs at a time. Selecting an API model stops the managed local engine. Turning the engine on, picking an
  engine model, or removing the selected API model makes the local model the agent's again.

## Where a model's figures come from

A model row is captured when added (`SetApiModelAdded`) and stored in `settings.json` `api_models`, so Pi can be
configured offline.

- Pi's pinned catalog (Pi 1.4.0, in the bundled runtime) sets the request format, output limit and price for models it
  knows. models.dev is fetched, and cached for a day in `<data>/agent/models-dev-opencode-go.json`, only when Pi lacks
  a listed OpenCode Go model.
- ChatGPT rows: the account's list (`chatgpt::listing`, `provider::from_chatgpt_listing`) sets the window
  (`context_window`), images (`input_modalities`) and thinking levels (`supported_reasoning_levels`); Pi gives format,
  output limit and price. A model Pi lacks takes format and output limit from a sibling of its family that Pi knows
  (Terra from Sol), never its price or name. `max_context_window` is not offered.
- Refresh: ChatGPT rows refresh from the account's list whenever it loads (`ApiModel::refreshed`,
  `UiAction::RefreshApiModels`). Only metadata changes; the running Pi keeps its configuration until its next start
  (a model, slider or thinking change, a reload, a crash restart). A catalog that knows less never overwrites what a
  row knows.
- An old ChatGPT placeholder row (128K, no output limit, text only) is repaired from its sibling on load and saved
  once (`provider::repair_placeholders`).

Windows as the providers listed them when last checked (data from the account and Pi's catalog, not constants):

| Model | Window | Output |
|---|---|---|
| GPT-6.1 Sol, GPT-6.1 Terra (ChatGPT) | 500,000 | 128,000 |
| GPT-6 Luna (ChatGPT) | 272,000 | from Pi |
| Claude Opus, Sonnet, Haiku 5.5 | 1,000,000 | 128,000 |

## Context budget

The budget is how many tokens one request may use; history compacts before it. It is not the window.

- Slider stops (`ApiModel::context_steps`): evenly spaced from 128K (`MIN_CONTEXT`) to the model's window, at most 8
  intervals (`MAX_CONTEXT_STEPS`); one stop when the window is 128K or less. A saved budget between stops is shown as
  its own stop. A slider move snaps to the nearest shown stop.
- An untouched slider means the model's whole window, on every provider and after every switch. A saved budget stays.
- A refresh keeps a saved budget exactly, as its own stop if needed, and never raises it. Only a window that shrank
  below it lowers it, to the largest stop that fits.
- Haiku 5.5 on Anthropic bills $0.10 / $0.50 per million tokens up to 100K input tokens per request and five times
  that above (`ApiModel::anthropic`). Derived: an untouched Haiku slider (1,000,000) lets requests pass that tier.
- Local engine: `engine_context` per engine model (`SetEngineContext`); missing means the engine's whole window.

## Limits: the four numbers

`provider::Limits::for_context(budget, max_output)`, one formula for local and API models:

- `response` (Pi's `maxTokens`): the model's output limit when known, else the reserve. Pi trims it per request to what
  the window leaves.
- `reserve`: budget / 8, clamped to 2,048 to 16,384, never above the output limit. It sets only where Pi compacts.
- `recent` (Pi's `keepRecentTokens`): min(budget / 4, 20,000).
- Thresholds: Heimdall compacts while idle at 85% of the budget (`COMPACT_AT_PERCENT`, `Limits::compact_at`). Pi's
  in-turn check fires at min(budget − reserve, 90% of the budget) (`IN_TURN_PERCENT`, `Limits::threshold`). Derived for
  a 500,000 budget: idle 425,000, in turn 450,000.
- `window`: Pi is told the model's real window, with `reserveTokens` = window − threshold, so the threshold is the
  budget's while a summary of a longer history still fits one request.
- Carry (`Limits::carrying`, `session::history_tokens`): after a switch to a smaller budget, a history past the new
  threshold that still fits the window raises Pi's in-turn threshold for that start, so the summary runs at the first
  quiet moment instead of in front of the next request. The carry ends at its first compaction.

When and how compactions run (the idle rule, silence limit, kill backoff): [crates/agent/README.md](../crates/agent/README.md).

## Thinking

Chosen per model in the menu; Low until chosen (`DEFAULT_THINKING`). `ApiModel::thinking_levels`:

- Claude Opus 5.5: Low to Max (thinking cannot be off). Sonnet and Haiku 5.5: Off to Max.
- A model whose provider list names efforts (ChatGPT): those levels, Off only where the list or Pi maps "none".
- Others: what Pi's catalog maps. Local engine: Off to High (`LOCAL_THINKING`).
- Pi starts with `--thinking <level>`; a change for the running model goes as `set_thinking_level` and applies from the
  next request, without a restart.
- Gotcha, Off on Anthropic's Sonnet and Haiku 5.5: Pi 1.4.0 sends Off as `thinking: {type: "disabled"}`, which Sonnet
  5.5 rejects. `crates/agent/runtime/provider-retry.mjs` with `thinking-policy.mjs` rewrites every Messages request on
  the wire, compaction summaries included: Sonnet `{type: "between_tools"}`, Haiku `{type: "disabled"}`, one top-level
  effort (Low's), no per-message effort messages.

## Speed

`agent_speed` in `settings.json`: `standard` (default) or `fast`, set through the `settings` tool; there is no menu
row. Fast makes `crates/agent/runtime/service-tier.mjs` add `service_tier: "priority"` to OpenAI Responses requests,
read per request. Each response's requested and reported tier goes to `logs/provider-tier.jsonl`. A reported `default`
after a priority request is logged as such; the log, not the request, says which tier served it.

## Background Tasks ▸ Model

Every background job runs on one model unless a task has its own (`settings.json` `background_model`, `task_models`),
on that model's own provider (`background_model::tool_endpoint`). The personal edition defaults to Claude Haiku 5.5 on
Anthropic (`ui::BACKGROUND_DEFAULT`); the public edition starts with none, and no job calls anything until one is
chosen. Without a credential there is no call and no fallback to another provider. A Claude Code instance in T3 is a
provider too (`t3:<instance>`). Each model may carry its own thinking level (`background_efforts`). With Agent off no
background request goes out.

The menu (Background Tasks ▸ Model) shows each task row with the short name of the model it really runs, an orange
arrow and the model answering now on a fallback, and the provider, account and level in its tooltip
(`ui::mac::background_picker`). Spent plans, fallback order, caching and the call path: [background-call.md](background-call.md).

## First-run downloads (public edition)

The public app bundles only small models (Phonon S, Silero VAD, the stock Smart Turn) and the helpers' runtimes (the
decider's MLX engine and metallib, the memory-search worker). Setup downloads the rest from
Hugging Face at pinned revisions into `<data>/models` ([home.md](home.md)). Sizes are the sums in
`model_downloads.lock.json`:

| Model | Lock group | Size | Needed for |
|---|---|---|---|
| Parakeet | `parakeet` | 632 MB | listening |
| Speakers (Nemotron 3 diarization, ReDimNet2 B6) | `speakers` | 158 MB | listening |
| Decider V6, 4-bit (default) / 8-bit / 16-bit | `decider-4bit`, `decider-8bit`, `decider-16bit` | 1.37 / 2.10 / 3.48 GB | listening |
| MossFormer2 | `night` | 91 MB | speech separation |
| EmbeddingGemma 2 | `embedding` | 1.53 GB | memory search |
| Qwen3-Reranker-0.6B | `reranker` | 1.20 GB | memory search (reranking) |

Total at the default precision: 4.98 GB (5.71 GB at 8-bit, 7.09 GB at 16-bit). Each precision is published, so the chosen one downloads directly; nothing is
converted on the Mac.

- The pins: `crates/app/src/model_downloads.lock.json` (each file's URL with its revision, size and SHA-256), written by
  `scripts/downloads_lock.py` from the listen, decide, embedding and speaker locks. `scripts/downloads_lock.py --check`
  says whether it is current; a test holds it to those locks.
- How (`model_store.rs`): one file at a time to `<path>.part`, resumed with an HTTP Range request, its size and SHA-256
  checked from disk, then renamed into place. Each request is capped at 90 s and the next resumes where it stopped; a
  checksum mismatch fails the file at once. A failed model retries by itself after 2 minutes, then twice as long each
  time up to 30 minutes, and the menu says why listening is off. One Heimdall downloads at a time (a lock file in
  `<data>/models`). A finished model is recorded in `<data>/models/downloads.json` and never fetched again.
  `<data>/models` links the bundled models from the app, so every loader reads one folder.
- When: Continue on Setup's listening page starts them (the precision is known then); they run while Setup continues.
  A later launch resumes any not finished. Getting ready shows one row per model with size, progress, a check when in
  place, and the reason with Retry when it failed. Start never waits.
- Listening stays off until Parakeet, the speaker models and a decider are in place, and starts by itself when they
  land, without a relaunch. A precision chosen later downloads; the nearest present one runs until it lands, then it
  swaps in.
- Tests use a local HTTP server; `HEIMDALL_NETWORK_TESTS=1` adds one real download of a small pinned Parakeet file.

### Test mirror

`HEIMDALL_MODEL_MIRROR=http://<host>:<port>` points every download at a cache server instead of Hugging Face (the VM
tests use one on the host). Only the origin changes: the server must serve the same paths
(`/<repo>/resolve/<revision>/<file>`), and every file is still checked against its pinned SHA-256, so a mirror can make
a run faster but never give it other bytes. A value with a path, or not `http(s)://`, is ignored. The app logs
`downloads: test mirror …` when one is in use.

## Memory search's local models

Two models serve memory search (`timeline_search`), both through the native helper in
`crates/archive/embedding/native`. Rule: a model runs at its own published precision. BF16 checkpoints run as BF16,
checked against BF16 tolerances; nothing runs in FP32 beyond weights of a few MB, and quantized weights are never
converted up.

| Model | Revision | Precision | Size | Job |
|---|---|---|---|---|
| EmbeddingGemma 2 | `google/embeddinggemma-2` @ `914f7f89` | BF16 | 1.53 GB | embeds chunks and queries |
| Qwen3-Reranker-0.6B | `Qwen/Qwen3-Reranker-0.6B` @ `e61197ed` | BF16 | 1.19 GB | reorders the best worded hits |

Both editions download the reranker at first run. When it is missing or cannot load, search keeps embedding order. Residency, memory caps and ranking: [chunk-index.md](chunk-index.md).

## See also

- [chatgpt.md](chatgpt.md): signing in with ChatGPT
- [settings.md](settings.md): every settings key, including the `settings` tool
- [background-call.md](background-call.md): the background call path, fallbacks and spent plans
- [inference-engine.md](inference-engine.md): the built-in local engine
- [chunk-index.md](chunk-index.md): timeline search and the reranker
- [editions.md](editions.md): what each edition bundles
