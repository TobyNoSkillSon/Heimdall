# Heimdall deciding layer

**Domain: decider.** The native d1 model (C ABI; Decider V6, V5 the rollback, on MLX) and the older 600M runtime: send / wait / not-for-Heimdall over a
2,000-token text window with the ack choice, suggest yes/no, and the two screen-command verdicts. Every utterance is archived
regardless of the verdict.
**Interface:** `LocalDecider`, `send::{Conversation, send_decision}`, `send_v3`, `names`, `suggest`, `barge` and `reply` (legacy, not asked),
`CommandCandidate`, `CommandSelection`, `Turn`. No workspace dependencies.
**Testing:** `cargo nextest run -p decide`; a few native parity tests take 1–3 s (fixtures in `tests/fixtures/`, need
`models/decide`). Two ignored GPU-fault tests. Research scripts: `lab/decide/scripts/`.

Command decisions and barge-in behaviour: `docs/decision-chain.md`.

## Model

- `d1::DEFAULT_MODEL` is `d1/d1-2B-heimdall-v6` (Decider V6, fine-tuned from V5, 4 Nov; `docs/MODELS.md`): a
  checkpoint folder (config.json, tokenizer.json, tokenizer_config.json, model.safetensors, calibration.json), 3.6 GB.
  V6 has V5's architecture, tokenizer and questions (`send_v3::Generation::V5` reads both), so the engine, prompts and
  memory figures below hold for both. Its runtime
  (`d1::DEFAULT_LIBRARY`) is `runtime-v5/libd1v5_engine.dylib` with `mlx.metallib` beside it: the d1v5 engine in
  Rust on MLX (`engine-v5/`, outside the cargo workspace so workspace builds never compile MLX). Build it with
  `scripts/build_decider_v5.sh` (nice 19, two jobs, offline). Locks: `scripts/models.lock.json` (model files) and
  `scripts/runtime-v5.lock.json` (dylib and metallib, rewritten by `build_decider_v5.sh --lock`).
- Version and precision (`precision.rs`): `DeciderModel` V6 (default) or V5 (the rollback), each in three folders, `d1-2B-heimdall-v6` (16-bit, the trained checkpoint), `-8bit` and `-4bit` (MLX affine
  group-64 quantization of the text trunk's linear layers; embedding and head BF16), the same for `-v5`; all six
  listed in `models.lock.json`; the weights are not in the repo (V6 is published at
  https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6, int8 default), 3.85 / 2.10 / 1.37 GB on disk per version. A `Pick` is one folder. The app loads the
  one in settings (`decider_model` "v6" or "v5", `decider_precision`) with
  `LocalDecider::load_pick`, and logs it: `decider: d1-2B-heimdall-v6-8bit (Decider V6, 8-bit) loaded in 0.4 s`.
  `LocalDecider::load` and `DEFAULT_MODEL` stay on V6's 16-bit for parity tests and replay tools. A missing folder
  resolves to the chosen version's nearest present precision (by memory), then to the other version (V6 missing: V5
  runs, the log and the menu say why; `precision::resolve`). Falling back beats failing: no decider means Heimdall
  stops hearing its name. Rolling back to V5 is `"decider_model": "v5"` (hot-swapped, no restart); undoing the
  install entirely is reverting the commit that added `DeciderModel`. Memory per instance, measured in the Rust engine (30 Oct): resident weights
  3.46 / 2.08 / 1.35 GB, peak while deciding 4.16 / 3.08 / 2.35 GB (the menu and Setup show the peak). The app holds
  one instance; a precision or version change hot-swaps it (`app/src/pipeline/decider.rs`).
- The engine runs its exact fast path (Metal kernels) and falls back to plain MLX by itself; `D1E_FALLBACK=1` forces
  plain. Its version string `d1v5-engine@…` is accepted beside the llama.cpp runtime's (`d1::check_version`).
- The 600M GGUFs (v1–v4) and their llama.cpp runtime (`scripts/build_native.sh`, `scripts/runtime.lock.json`) stay
  loadable for parity tests and replay; they are no longer bundled. Rolling back is reverting the commit that switched
  `DEFAULT_MODEL` and the bundle flags.
- `send_v3::Generation` reads the version from the model's file or folder name: v5 and later ask
  `questions/dispatch-v5.json` (byte for byte the V5 training rows' question: v3's English ack question, for every
  sentence, never renamed) after the names header (`send::Snapshot::fit_v5`), every V5 ack key an `en_` voicebank clip; v3/v4 get the per-language
  `questions/dispatch-v3.json`; v2 and earlier `send::question`.
- V5's command check (look at this, take a screenshot) reads V5's conversation layout (`StateLayout::V5`,
  `DecisionInput::conversation_text`): header, recent turns (the app's 20), "Agent: idle", pending speech ending with
  the command sentence. V5's suggest question (`suggest`, `questions/suggest-v5.json`, byte for byte its rows) uses
  the wake's layout (`fit_v5`); the suggester asks it after every line (`docs/suggester.md`). V5 never learned
  barge-in, the reply line, the fixed command, candidates or cancel-task, and since 29 Oct the app asks none of them
  (`barge`, `reply` and those question kinds remain for parity tests and old records).
- `names` (Personalise, 26 Oct): the configured assistant and user names replace `Heimdall` and the owner's name in context and the
  wake spotter. V5 always gets the header and never renamed questions; for v4, default names stay byte-identical
  (no header) and other names also rewrite the questions.
- Real-row check (opt-in, loads V5's 2B model): `HEIMDALL_V5_ROWS=…/data/v5/test.jsonl cargo nextest run -p decide
  --run-ignored only v5_rows`. V6 load check (opt-in, the app's path, five sentences, no scores):
  `cargo nextest run -p decide --run-ignored only v6_loads` (`HEIMDALL_V6_PICK=16bit|8bit|4bit`, 4-bit by default).

## Send decision

`send::Conversation` and `send_decision` make the text-only send / wait / not_for_heimdall decision over a 2,000-token
window (`send::WINDOW`), recomputed on each final. Oldest turns drop first; unsent text stays in the prompt unless it
alone exceeds the window, when only its newest suffix is inferred. The app restores the conversation from its archive
before accepting finals and rehydrates unsent observations into the agent spool by their original ids. A wait or
not_for_heimdall never consumes pending text. Typed messages send immediately and form barriers.

Host wakes besides d1's argmax (`send.rs`):

- The assistant's name as an address (Heimdall also by the spotter's `KNOWN_ONE_WORD` spellings, other names within
  an edit budget by length).
- "Send it" when it opens its clause ("Okay, send it now."), follows a request frame ("can you send it") or joins
  another command ("write the reply and send it"); never inside talk about sending.
- No follow-up window since 29 Oct (decision 33): speech right after Heimdall's playback goes to d1 like any sentence.
- During a call (another participant's line within `CALL_ACTIVE_NS`, 120 s), "send it" and a
  name that is not an address wait for d1 and wake only when `not_for_heimdall` is below `CALL_NOT_FOR` (0.5). The user
  addressing Heimdall by name wakes at once (`addressed_by_name`).
- In the minute after a Heimdall question card appears, d1 still judges and sends at a lower score
  (`Trigger::OpenQuestion`, `app/src/bridge/questions.rs`).

There is no silence wake, send head, addressedness model, threshold setting or decider switch. `Question::SendNow`
remains only for historical tensor parity and calibration replay; the app never calls it.

## Runtime and evidence

- Send inference and the suggest question after it run on a bounded FIFO worker (32 outstanding sentences). Explicit
  cues enqueue their wake at once. Failed or saturated inference leaves speech pending and journals a problem. One owner
  thread holds the app's only d1 instance; commands use its higher lanes (they run before queued sends, never in parallel).
- Commands, the wake with its ack choice, and suggest share the d1 runtime. Native slots keep a 16k
  media budget. The encoder is non-causal: no KV cache.
- `send_decision` records hold the exact request, token counts, distribution, trigger, outcome, latency, trimming and
  audio reference; typed sends record inference as bypassed. `heimdall-archive timeline --problems` flags failures,
  inference above 500 ms and window truncation. Other questions journal `d1_question` (old archives: `d1_tcl`).

## Verification

`heimdall-send-replay` and `lab/decide/scripts/replay_send.py` run the production send path on archive exports (four-minute
budget, 4 GiB memory ceiling, no swap growth) and never open the live profile. History: phase-one experiments in
`lab/history/docs/decide/context-prototype.md`, multimodal speed work in `lab/history/docs/decide/D1_SPEED.md`.
