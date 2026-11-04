# heimdall-night — overlap separation (speech split)

Separates two-way overlapping speech in the archive with MossFormer2 and re-transcribes and identifies each track;
the app runs the same code during calls (speech split).

**Interface:** modules `job` (process/run and the model worker), `gate` (AC and input idle, background QoS),
`intake`, `dataset`, `separator`, `correction` (the speech-split CPU helper and its guard), `storage` (disk floor,
owned scratch), `metrics`; binary `heimdall-night` (`gate`, `process`, `snapshot`, `run`, `evaluate`). Depends on
`heimdall-archive`, `listen`, `speakers`.

**Testing:** `cargo nextest run -p heimdall-night`. Ignored tests: two subprocess helpers launched by the recovery
and live-floor tests, and `contracts.rs`'s reproduction that holds 4.2 GiB and runs the CPU models for about 40 s.
