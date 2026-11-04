# Listen provenance and attribution

- **Parakeet Ultra weights:** moondream / Moondream Inc.,
  https://huggingface.co/moondream/parakeet-ultra, CC-BY-4.0.
  Core ML conversion: FluidInference,
  https://huggingface.co/FluidInference/parakeet-ultra-coreml,
  revision `95eaa59a39d4394f047a4dc5cce480388a60d1b6`. The int8 encoder,
  fp16 decoder/joint and CPU preprocessor are used unchanged. Rust orchestration
  is original; no model weights are distributed in Git.
- **FluidAudio algorithm reference:** FluidInference / FluidAudio contributors,
  https://github.com/FluidInference/FluidAudio,
  revision `e560c6445ffdc42e60c7b282d162aae271f18e2a`, Apache-2.0.
  Reviewed TDT state/duration handling, Core ML contracts and emission-delay
  correction. No FluidAudio library or Swift source is bundled or executed.
- **Vella Metal kernel port:** `src/metal/vella_formats27.metal` vendors
  `lab/models/Shared/vk-formats27/kernels.metal` from Vella revision
  `d7fa658db6f1370944694648510786a6c9e99821`, MIT (2026 Vella contributors).
  `src/metal/Vella-LICENSE` retains its notice. Heimdall adds broadcast batch-plane
  offsets, FP16-boundary-preserving bias/ReLU epilogues and direct NCHW stores
  to the FP16 tensor GEMMs; only `biga_f16` and `tile4_f16` are used. The graph
  executor and other shaders are Rust/Metal ports of the pinned model operations,
  informed by Vella's Swift/MLX runtime. There is no Vella runtime dependency.
- **Silero VAD:** Silero Team / snakers4,
  https://github.com/snakers4/silero-vad,
  revision `1e261b036686cd0017d500ee96acd1c4ba572a9d`, MIT. Official current
  `src/silero_vad/data/silero_vad.onnx`; the model's file digest is pinned.
- **Smart Turn v3.2:** Pipecat AI,
  https://huggingface.co/pipecat-ai/smart-turn-v3,
  revision `f766f81d3cfdf7737ac64aad813d91bbfd56bf93`, BSD-2-Clause.
  Explicit `smart-turn-v3.2-cpu.onnx`, not inference.py's older v3.1 example.
  Frontend semantics verified against
  https://github.com/pipecat-ai/smart-turn/tree/4786657e242dfe77dd138699ac564ee074a2a543
  and Transformers 4.57.6 `WhisperFeatureExtractor` numpy implementation.
- **FLEURS fixtures:** Google FLEURS (Conneau et al., 2022),
  https://huggingface.co/datasets/google/fleurs,
  revision `70bb2e84b976b7e960aa89f1c648e09c59f894dd`, CC-BY-4.0.
  Four short public EN/PL files were retrieved from the existing verified Vella
  corpus (file hashes verified), converted to mono 16 kHz s16 WAV. The synthetic
  mixed file concatenates those excerpts with zero silence. Specific original
  archive members, references and converted WAV checksums are in the fixture
  manifest. Synthetic DSP fixtures are generated, except speech taken from en-2.

Rust crate/runtime dependencies retain their published MIT/Apache/Zlib or other
Cargo registry notices. `ort` 2.0.0-rc.13 / ONNX Runtime is statically linked;
`objc2-core-ml` 0.3.2 binds the system Core ML framework directly. Include these
notices and the model/data attribution when bundling the app.
