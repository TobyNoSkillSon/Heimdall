# Loading d1v5-engine from Heimdall

Not done here. Whether Heimdall loads this engine is a maintainer decision. This file lists the exact Heimdall-side change it needs.

The engine's dylib exports every symbol `crates/decide/src/d1.rs` resolves, with the same C signatures:
`heimdall_d1_create_text_backend`, `heimdall_d1_decide`, `heimdall_d1_decide_interruptible`, `heimdall_d1_tokens`,
`heimdall_d1_free`, `heimdall_d1_destroy`, `heimdall_d1_version`, `heimdall_d1_recreate_device` and
`heimdall_d1_test_fail_next`. The reply JSON has the shape `NativeD1::parse`, `distribution`, `ensure_max` and
`send_v3::parse` read. These are the things that stop Heimdall loading it today:

1. **Version checks** (`NativeD1::load`). Heimdall requires a version that starts with
   `llama.cpp@62a6f74b...;` and contains `;native-ownership-v1`, `;async-owner-v1` and `;text-window-v1`. This
   engine reports `d1v5-engine@0.1.0;mlx-rs-0.32;text-only;json-v1`. The change is to accept either runtime:

   ```rust
   let d1v5 = version.starts_with("d1v5-engine@");
   ensure!(d1v5 || version.starts_with("llama.cpp@62a6f74bab748b1755f4cd8dfc0871caf865c6c8;"), "unexpected native ABI version");
   if !d1v5 {
       ensure!(version.contains(";native-ownership-v1"), "d1 runtime lacks native ownership isolation");
       ensure!(version.contains(";async-owner-v1"), "d1 runtime lacks asynchronous owner ABI");
       ensure!(version.contains(";text-window-v1"), "d1 runtime lacks bounded text slot ABI");
   }
   ```

   The guarantees behind those markers, as this engine provides them:
   - Ownership: one process-wide lock serialises all MLX work, so any thread may call, one at a time.
   - Async owner: `heimdall_d1_decide_interruptible` polls the callback before each question's forward.
   - Text window: `heimdall_d1_tokens` counts with the checkpoint's own tokenizer.

   If the maintainer prefers the markers to be printed, add them to `ffi::VERSION` instead of changing the checks.
2. **Model path** (`DEFAULT_MODEL`, `D1::load`). Heimdall passes a GGUF file
   (`d1/d1-omni-600M-heimdall-v3-F16.gguf`). This engine takes a d1-2B checkpoint folder (config.json, tokenizer
   files, model.safetensors, calibration.json), for example `.../runs/v5/final`. `DEFAULT_MODEL` becomes that folder.
   The library path becomes `libd1v5_engine.dylib`, with `mlx.metallib` copied beside it.
3. **Generation** (`send_v3::Generation::of`). This reads `heimdall-vN` from the model's file name and uses V3 only
   for N >= 3. A folder named `final` would read as V2. Either name the shipped folder with `heimdall-v5`, or map
   the d1v5 runtime to V3 explicitly. The V5 dispatch question is the V3 shape, keyed `dispatch`.
4. **Tokens per question.** Heimdall checks `usage.input_tokens <= send::WINDOW` (2,000). Here `input_tokens` is the
   longest single prompt, because each question is read alone. Heimdall's fitting (`fit_text` via
   `heimdall_d1_tokens`) measures the state with the same tokenizer, so the bound means the same thing.
5. **Several questions in one request.** The 600M runtime packs questions after one state. This engine reads each
   question as its own prompt, which is d1's single-question layout and matches how V5 was trained (one question
   a row). Answers to multi-question requests can differ slightly from a packed read.

Unchanged: `heimdall_d1_recreate_device` returns true, and `heimdall_d1_test_fail_next` makes the next decide fail
with `HEIMDALL_GPU_FAULT`, which `gpu_recovery` recognises. Engine errors that name Metal or the GPU carry the same
marker.
