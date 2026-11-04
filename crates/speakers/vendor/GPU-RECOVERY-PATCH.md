# Heimdall GPU recovery patch

The pinned GGML source retains its backend ABI. The private
`ggml_metal_synchronize` now returns `bool`; the added public
`ggml_backend_metal_synchronize_checked` exposes that result without changing the
upstream void synchronization callback. Every pending command buffer, including
transfer buffers, is awaited and checked. Errors stay sticky until reconstruction.
NeMo checks before host readback and after asynchronous transfers; it throws
`HEIMDALL_GPU_FAULT` before returning any result.

Sortformer retries a single current chunk/batch before AOSC stream state changes.
All streams share the original model and one recovery mutex. Reconstruction drops
Session graphs, buffers, backend contexts, queues and pipelines, then reacquires
the device. A second failure uses Q8 CPU for 30 seconds before retrying GPU.
The frontend remains the original CPU FFT; no audio-history replay is needed.

Device reset requires zero active backend contexts. GGML's device-owner vector is
updated along with the registry pointer, preventing stale ownership/double-free.
Rust custom projection buffers and pipelines are cleared/rebuilt too. A disabled
custom owner cannot intercept CPU work. The status API is
`speakers::gpu_recovery::gpu_status()`; the helper transports this status with every reply, and
`CallScopedSpeakers::gpu_status()` exposes it in the parent. Menu rendering remains
app wiring outside these owned crates.

Offline fault fixtures cover rejected finite readback, recreation, CPU fallback,
GPU return and six concurrent streams on one shared Nemotron model. Production
Rust hooks always return false; only test builds arm injections or expire backoff.
