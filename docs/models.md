# Agent models: providers, windows, context budget, thinking, speed

The single home for which models the agent can run, how big each request may get and when history compacts.
Connect flows live elsewhere: ChatGPT in `docs/chatgpt.md`, OpenCode Go and Anthropic keys in `crates/agent/README.md`
("API providers"). The menu's layout is in `crates/ui/README.md`. Code: `crates/agent/src/provider.rs`
(`PROVIDERS`, `ApiModel`, `Limits`), `crates/agent/src/runtime.rs` (`configure`, `configure_api_carrying`).

## Providers

`provider::PROVIDERS`, in menu order (subscriptions first, then this Mac, then API keys):

| Id | Name | Credential | Notes |
|---|---|---|---|
| `openai` | ChatGPT | Sign in with ChatGPT (browser only) | Subscription. Models from the account's list. No hour cache. |
| `opencode-go` | OpenCode Go | Console device sign-in, or a key | Subscription. Models from `/v1/models`; models.dev fills in what Pi's catalog lacks. |
| `anthropic` | Anthropic | API key (+ workspace for `sk-ant-usr-…`) | One-hour prompt cache (`hour_cache`). A first key selects Claude Opus 5.5. |
| `openrouter` | OpenRouter | API key | Chat completions. |
| `openai-compatible` | OpenAI-Compatible | URL + key | Not offered in Add Provider any more (Own Engine replaces it); a connected one stays. |
| `own-engine` | Own Engine | URL, key optional | Any local server. |

- Each provider is its own billing route. Never mix a subscription with a metered API and never fall back from one to
  another. A first-class metered "OpenAI API" provider is not built yet.
- The default agent model, for new and old profiles, is Claude Haiku 5.5 on OpenCode Go (`AgentModel::default`;
 ). Default added rows: Haiku on OpenCode Go plus Opus, Sonnet and Haiku 5.5 on Anthropic.
- Signing in to ChatGPT never switches the agent's model. A first Anthropic key does.

## Where a model's figures come from

A row is captured when added (`SetApiModelAdded`) and stored in `settings.json` `api_models`, so Pi can be configured
offline.

- Pi's pinned catalog (Pi 1.4.0, in the bundled runtime) decides the request format, output limit and price for
  models it knows. Pi 1.4.0 describes OpenCode Go's models itself, so models.dev is fetched (and cached a day in
  `<data>/agent/models-dev-opencode-go.json`) only when Pi lacks a listed model.
- ChatGPT rows: the account's own list (`chatgpt::listing`, `provider::from_chatgpt_listing`) decides the window
  (`context_window`), images (`input_modalities`) and thinking levels (`supported_reasoning_levels`). Pi gives format,
  output limit and price. A model Pi lacks (Terra) takes format and output limit from a family sibling Pi knows (Sol),
  never its price or name.
- Refresh: ChatGPT rows refresh from the account's list whenever it loads (`ApiModel::refreshed`,
  `UiAction::RefreshApiModels`). Metadata only: the running Pi keeps its configuration; the next start (model, slider
  or thinking change, reload, crash restart) uses the new figures. A catalog that knows less never overwrites what a
  row knows.
- The old Terra placeholder (128K, no output limit, text only) is repaired from Sol on load and saved once
  (`provider::repair_placeholders`).

Windows as listed or catalogued (read from the account and Pi 1.4.0's catalog, 27 Oct 2026):

| Model | Window | Output | Source |
|---|---|---|---|
| GPT-6.1 Sol, GPT-6.1 Terra (ChatGPT) | 500,000 (`max_context_window` 872,000 not offered) | 128,000 | account list; Pi (Terra via Sol) |
| GPT-6 Luna (ChatGPT) | 272,000 (872,000 not offered) | from Pi | account list |
| Claude Opus, Sonnet, Haiku 5.5 | 1,000,000 | 128,000 | Pi catalog, Anthropic price list |

The 872,000 maximum is untested and probably spends more of the plan per request (a default;
the owner may change it). Pi's own catalog says 272,000 for Sol; the account's 500,000 wins.

## Context budget

The budget is how many tokens one request may use; history compacts before it. It is not the window.

- Slider stops (`ApiModel::context_steps`): evenly spaced from 128K (`MIN_CONTEXT`) to the model's window, at most 8
  intervals; one stop when the window is 128K or less. a saved budget is shown as its own stop when it falls
  between them. A slider move snaps to the nearest shown stop (`snap`).
- Untouched slider: the model's whole window, on every provider and after every switch ; a budget the user saved
  stays theirs. Derived: the Claude 5.5 models 1,000,000 (501,714 before, the stop nearest the old 500K default); Sol
  and Terra 500,000; Luna 272,000.
- A refresh keeps the saved budget exactly, as its own stop if needed (Sol's old 272K inside the new 500K window
  stays 272K). It never raises it. Only a window that shrank below it lowers it, to the largest stop that fits.
- Haiku 5.5 bills $0.10/$0.50 per million up to 100K input tokens per request and $0.50/$2.50 above. Derived: an
  untouched Haiku slider (1,000,000) lets requests pass that tier.

## Limits: the four numbers

`provider::Limits::for_context(budget, max_output)`, one formula for local and API models:

- `response` (Pi's `maxTokens`): the model's own output limit when known, else the reserve. Pi trims it per request to
  what the window leaves.
- `reserve`: budget / 8, clamped to 2,048–16,384, never above the output limit. It sets only where Pi compacts.
- `recent` (Pi's `keepRecentTokens`): min(budget / 4, 20,000).
- Thresholds: Heimdall compacts while idle at 85% of the budget (`COMPACT_AT_PERCENT`, `compact_at`).
  Pi's own in-turn check fires at min(budget − reserve, 90% of the budget) (`IN_TURN_PERCENT`, `threshold`).
  Derived for a 500,000 budget: idle 425,000, in-turn 450,000.
- `window`: Pi is told the model's real window, with `reserveTokens` = window − threshold, so the threshold is the
  budget's while a summary of a longer history still fits one request.
- Carry (`Limits::carrying`, `session::history_tokens`): after a switch to a smaller budget, a history past the new
  threshold that still fits the window raises Pi's in-turn threshold for that start, so the summary runs at the first
  quiet moment instead of in front of the next request. The carry ends at its first compaction.

When and how compactions run (idle rule, silence limit, kill backoff) is in `crates/agent/README.md`.

## Thinking

Per model, chosen in the menu; Low until chosen (`DEFAULT_THINKING`; "think less and just answer right
away"). `ApiModel::thinking_levels`:

- Claude Opus 5.5: Low to Max (thinking cannot be off). Sonnet and Haiku 5.5: Off to Max.
- A model whose provider list names efforts (ChatGPT): those levels, Off only where the list or Pi maps "none".
- Others: what Pi's catalog maps.
- Pi starts with `--thinking <level>`; a change for the running model goes as `set_thinking_level` and applies from the
  next request, no restart.
- Off on Anthropic's Sonnet and Haiku 5.5: Pi 1.4.0 sends Off as `thinking: {type: "disabled"}` with no effort, which
  Sonnet 5.5 rejects. `runtime/provider-retry.mjs` with `runtime/thinking-policy.mjs` rewrites every Messages request on
  the wire, compaction summaries included: Sonnet `{type: "between_tools"}`, Haiku `{type: "disabled"}`, one top-level
  effort (low), no per-message effort messages.

## Speed

`agent_speed` in `settings.json` (`standard` default, `fast`), set only on request through the `settings` tool;
no menu row. Fast makes `runtime/service-tier.mjs` add `service_tier: "priority"` to OpenAI Responses requests, read
per request. Every response's requested and reported tier goes to `logs/provider-tier.jsonl`. On 26 Oct the ChatGPT
subscription reported `default` on every priority request, so a priority request is logged as "Fast requested /
default reported", never as proof Fast served it.

## Background Tasks ▸ Model

Every background job (plan refresher, day timeline, voice namer, suggester, deep search) runs on one model
(`settings.json` `background_model`; default Claude Haiku 5.5 on Anthropic, `ui::BACKGROUND_DEFAULT`), on that
model's own provider
(`app::background_model::tool_endpoint`). No credential: no call, never a fallback to another provider. A Claude Code
instance in T3 is a provider too (`t3:<instance>`, the subscription, no key): one short-lived thread a call, no tool
loops (docs/background-call.md). Each model may carry its own thinking level (`background_efforts`).

What the menu shows (one line per row): each task row's value names
the model it really runs, resolved through its own pick or the Default Model, as a short name ("Haiku 5.5", "Luna");
on a fallback, an orange arrow and what answers now. A row that chooses a model (Default, a task's own)
shows the provider's mark before the name; rows that take the default show the name alone. Provider, account and
level are each row's tooltip; the Default row's tooltip names the tasks it serves. A task's picker goes back to the
shared model through Use Default Model, which names it (`ui::mac::background_picker`).

- With Agent off no background request goes out (`background_gate::check`).
- Spent plan or credit (`background_gate`, audit 27 Oct F8): once the agent's quota watch or a background answer says
  a provider's limit is spent, no background request goes to it until a request there succeeds or the stated reset
  passes. A reset named by an earlier warning counts. Without a stated reset one job probes every 30 minutes
  (`QUOTA_RETRY_MS`).
- Responses requests (the ChatGPT route) carry `prompt_cache_key` `heimdall-<job>-<prompt hash>` (model, effort,
  schema, system), and the same key in the `session_id` and `x-client-request-id` headers, as Pi sends its session on
  that route. Each job's runs share one cache identity; whether the route honours it is measured in live traffic only
  (27 Oct night: with the body key alone, Luna's plan passes read 0 or 8,448 tokens of byte-identical prefixes). The
  route takes no `prompt_cache_retention`, so passes more than a few minutes apart may still miss. A job's effort follows the model's listed
  levels. The answer is read from the streamed `output_item.done` items in output order, because the completed event
  on this route lists no output.
