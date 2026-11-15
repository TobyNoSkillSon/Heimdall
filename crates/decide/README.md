# decide

The deciding layer: loads the d1 decider (a 2B model on MLX, through a C ABI dylib) and asks it the wake question
(send, wait or not for Heimdall, with the acknowledgement line), the suggest question and the two screen-command
checks. Behaviour is in [docs/decision-chain.md](../../docs/decision-chain.md).

Read this when: you want to find the decider's code, change a question or the window fit, change how the model loads,
or run the decide tests.
Code: `crates/decide/src/`, `crates/decide/questions/`, `crates/decide/engine-v5/` (the engine, outside the workspace).

## Files

| file | what it holds |
|---|---|
| `src/lib.rs` | `LocalDecider` (`load`, `load_pick`, `send_decision_interruptible`, `suggest_decision_interruptible`, `command_decision_at`), `DecisionInput`, `Turn`, `Question`, `Answer`, limits (`MAX_CONTEXT` = 20 turns) |
| `src/d1.rs` | the dylib loader `D1`: `DEFAULT_MODEL` (`d1/d1-2B-heimdall-v6`), `DEFAULT_LIBRARY` (`runtime-v5/libd1v5_engine.dylib`), `check_version` (accepts only `d1v5-engine@…`) |
| `src/send.rs` | `Conversation`, `Snapshot`, `WINDOW` (2,000 tokens), `Trigger`, `explicit_trigger`, `addressed_by_name`, the call rules (`CALL_ACTIVE_NS`, `CALL_NOT_FOR`, `farewell`, `off_call`, `says_back`), `d1_gate` and `NEAR_TIE` |
| `src/send_v3.rs` | the wake question (`questions/dispatch-v5.json`), its fit (`fit_v5`) and parse (`parse_v5`), `Generation` |
| `src/suggest.rs` | the suggest question (`questions/suggest-v5.json`) |
| `src/command.rs` | `CommandCandidate`, `CommandIntent`, `CommandSelection`, `command_chain_at`, `COMMAND_POLICY` |
| `src/precision.rs` | `DeciderModel` (V6 default, V5), `Precision` (16-bit, 8-bit, 4-bit default), `Pick`, `resolve` (missing-folder fallback) |
| `src/names.rs` | the configured assistant and user names in the context header and the wake spotter |
| `src/archive.rs` | `TrainingArchive`, `ArchiveSink`: the append-only decision archive |
| `src/bin/heimdall-send-replay.rs` | replays the production send path on a JSONL export (arguments `LIBRARY MODEL PROJECTOR 2000`); opens no profile |
| `src/bin/heimdall-command-replay.rs` | replays command decisions on a JSONL export |
| `questions/` | `dispatch-v5.json` (the wake), `suggest-v5.json`, `dispatch-v3.json` (read only by `crates/voicebank/src/acks.rs` for the acknowledgement-line check) |
| `crates/decide/scripts/fetch_models.py` | downloads and checks the decider files listed in `crates/decide/scripts/models.lock.json` into `models/decide` |
| `crates/decide/scripts/runtime-v5.lock.json` | size and SHA-256 of the placed `libd1v5_engine.dylib` and `mlx.metallib` |
| `crates/decide/scripts/runtime.lock.json` | still read by `scripts/bundle_models.py` |

Questions are sent byte for byte as the model was trained on them; do not reword them.

`src/lib.rs` also compiles `crates/speak/src/gpu_recovery.rs` in through `#[path]`; the crate has no workspace
dependencies in `Cargo.toml`.

## The runtime

The app loads `models/decide/runtime-v5/libd1v5_engine.dylib` with `mlx.metallib` beside it; `HEIMDALL_D1_LIBRARY`
overrides the path. Build and place it with `scripts/build_decider_v5.sh` (`--lock` also rewrites
`crates/decide/scripts/runtime-v5.lock.json`). The engine's code map: [engine-v5/README.md](engine-v5/README.md).

## Tests

- `cargo test -p decide` (the full check runs it under `cargo nextest run -p decide`): no model is loaded.
- Model tests are ignored by default and run one at a time, each loading a 2B decider from `models/decide`:
  `cargo nextest run -p decide --run-ignored only <name>`. Their ignore reasons start with `model:` or `lab:` or say
  what they load.
  - `tests/v6_load.rs` (`v6_loads`): the app's load path on five sentences; `HEIMDALL_V6_PICK=16bit|8bit|4bit`,
    default 4-bit.
  - `tests/v5_rows.rs` (`v5_rows`): V5 against labelled rows; needs `HEIMDALL_V5_ROWS` pointing at a `test.jsonl`.
  - `tests/models.rs` and the ignored tests in `src/d1.rs`: V6 16-bit on the engine; the `lab:` ones also need
    `HEIMDALL_D1_LIBRARY` set to a freshly built dylib.
- `tests/api.rs`, `tests/names_parity.rs`: no model.

## See also

- [docs/decision-chain.md](../../docs/decision-chain.md): wake, calls, commands, barge-in, labels and evidence
- [docs/suggester.md](../../docs/suggester.md): the suggest question and cards
- [docs/settings.md](../../docs/settings.md): `decider_model`, `decider_precision`
- [docs/models.md](../../docs/models.md): the decider's files, sizes and sources
- [engine-v5/README.md](engine-v5/README.md): the MLX engine behind the dylib
