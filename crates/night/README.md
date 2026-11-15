# heimdall-night: overlap separation and training export

Separates two-speaker overlapping speech with MossFormer2, re-transcribes and identifies each track, and writes the
result as training material. The app runs the same code during calls as the speech split.

Read this when: you change the speech split's CPU helper, the overlap jobs, their disk floor or the `heimdall-night` CLI.
Code: `crates/night/src/`, `crates/app/src/speech_split.rs`, `crates/app/src/pipeline/speech_split.rs`,
`crates/app/src/night_runtime.rs`.

## Modules

| Module | Owns |
|---|---|
| `job` | one isolated model worker at a time; its immutable output is the durable checkpoint |
| `gate` | fail-closed run gate: AC power and 300 s of input idle (`IDLE_SECONDS`), background QoS; requests no permission |
| `intake` | metadata-first discovery; no archive snapshot or whole-archive WAV expansion |
| `dataset` | latest diarization revisions turned into bounded overlap jobs |
| `separator` | MossFormer2 separation (`separator::MODEL_ID`) |
| `correction` | the call-scoped speech-split CPU helper (`Client`, `helper`) and its memory guard |
| `storage` | disk floor and owned scratch; night writes never touch the live archive reserve |
| `metrics` | normalised word edit distance and two-speaker cpWER |
| `bug_notes`, `bug_scout` | the bug-note inbox; used only by the personal edition ([editions.md](../../docs/editions.md)) |

Binary `heimdall-night` (`src/main.rs`): `gate`, `process`, `run`, `worker`, `evaluate`, `enroll-fixture`,
`--speech-split-helper`, and `bugs list|scout|set-cause|export-decider-cases|resolve|check|reconcile`.
Depends on `heimdall-archive`, `heimdall-home`, `listen`, `speakers`. Model licence and card: `third-party/`.

## Tests

- `cargo test -p heimdall-night`.
- Ignored on purpose: `checkpoint_writer_helper` in `src/job.rs` (a subprocess the `abrupt_exit_resume` test launches)
  and `cpu_helper_survives_a_parent_above_the_old_combined_budget` in `tests/contracts.rs`, which holds 4.2 GiB and
  runs the CPU models for about 40 s.

## See also

- [models.md](../../docs/models.md): the MossFormer2 download and pin
- [speakers.md](../../docs/speakers.md): diarization and naming the tracks feed
- [archive.md](../../docs/archive.md): the archive the jobs read and the free-space floor
