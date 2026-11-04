# Third-party notices

Scope: the Heimdall Alpha public edition, as built from `scripts/bundle_models.py` (public), the speakers, listen, decide, speak, night and archive crates, and the agent runtime. The personal-only pieces are listed at the end and are not in the public build.

Full licence texts are in `licenses/`. Each file names its source URL, retrieval date (2026-10-27) and SHA-256 at the top. The root `LICENSE` is Apache-2.0. `NOTICE` carries the attribution text and the Apache-2.0 NOTICE files.

Obligation keys: **attribution** (credit in NOTICE), **notice kept** (copyright notice must ship), **licence copy** (licence text must ship), **terms** (use restriction or condition on the weights).

## Bundled engines and runtimes

- **Node.js 26.8.1**. Licence: Node.js licence (MIT-style, with bundled-dependency notices). Text: `licenses/Node.js-LICENSE.txt`. Obligation: licence copy.
- **Pi agent packages 1.4.0**: `@earendil-works/pi-coding-agent`, `pi-agent-core`, `pi-ai`, `pi-codemode`, `pi-mcp`, `pi-telemetry`, `pi-tui`. Licence: MIT, Copyright (c) 2025 Mario Zechner. Text: `licenses/Pi-MIT.txt`. Obligation: licence copy, notice kept.
- **Splash native inference engine 1.4.1** (`splash` engine binary and `splash.metallib`, from `incoai/splash`). Licence: Apache-2.0. Text: `licenses/Apache-2.0.txt`; the Splash LICENSE is byte-identical. Notices: `licenses/Splash-THIRD_PARTY_NOTICES.txt` (llama.cpp/ggml among them). Obligation: licence copy, notice kept. Bundled in `Contents/Resources/splash/engine`; Splash's Python server is not shipped (Heimdall's Rust `heimdall-splash` replaces it), so none of its Python, Tcl or Tk components ship.
- **ONNX Runtime** (statically linked through `ort` 2.0.0-rc.13). Licence: MIT, Copyright (c) Microsoft Corporation. Text: `licenses/ONNX-Runtime-MIT.txt`. Version not verified. Obligation: notice kept.
- **ort** 2.0.0-rc.13 and ort-sys. Licence: MIT OR Apache-2.0. Texts: `licenses/Apache-2.0.txt` and `licenses/ONNX-Runtime-MIT.txt`. Obligation: licence copy for the chosen option.
- **llama.cpp and ggml** (in `libheimdall_decide.dylib`, pinned `62a6f74b`, and the ggml CPU and Metal backends in `crates/speakers/vendor/ggml`). Licence: MIT, Copyright (c) 2023-2026 The ggml authors. Text: `licenses/llama.cpp-ggml-MIT.txt`. Obligation: notice kept.
- **NeMo-Speech.cpp** (diarization code, `crates/speakers/vendor/nemo-speech`). Licence: Apache-2.0, Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. Texts: `licenses/NeMo-Speech-NVIDIA-Apache-2.0.txt`, and its NOTICE in `NOTICE`. Obligation: licence copy, NOTICE kept.
- **mlx-swift 0.32.3** (native embedding worker, `crates/archive/embedding/native`). Licence: MIT, Copyright (c) 2023 ml-explore. Text: `licenses/MLX-Swift-MIT.txt`. Obligation: notice kept.
- **mlx** (C++ core built by mlx-swift). Licence: MIT, Copyright (c) 2023 Apple Inc. Text: `licenses/MLX-MIT.txt`. Version inside 0.32.3 not verified.
- **mlx v0.32.2, mlx-c, mlx-rs 0.32.0 and mlx-sys 0.6.0** (statically in `libd1v5_engine.dylib`, the Decider V5 engine, with `mlx.metallib` beside it), plus the Rust crates in `crates/decide/engine-v5/Cargo.lock` (tokenizers 0.23, serde, regex, half, anyhow). Licences: MLX MIT, Copyright (c) 2023 Apple Inc. (`licenses/MLX-MIT.txt`); mlx-rs and mlx-sys MIT OR Apache-2.0; tokenizers Apache-2.0 (`licenses/Apache-2.0.txt`). Obligation: notice kept. Per-crate licences of the engine's dependency tree were not traced.
- **mlx-swift-lm** at `c53d7d48`. Licence: MIT, Copyright (c) 2024 ml-explore. Text: `licenses/MLX-Swift-LM-MIT.txt`. Obligation: notice kept.
- **Other Swift packages resolved by the embedding worker** (`crates/archive/embedding/native/Package.resolved`). swift-transformers 150169bf and swift-jinja 2.5.1: Apache-2.0 (`licenses/Apache-2.0.txt`). swift-crypto 4.5.2 and swift-asn1 1.7.3: Apache-2.0, NOTICE files reproduced in `NOTICE` (`licenses/swift-asn1-LICENSE.txt`). swift-collections 1.7.3, swift-numerics 1.1.1, swift-syntax 603.0.2: Apache-2.0 with Runtime Library Exception (`licenses/Swift-Apache-2.0-runtime-exception.txt`). yyjson 0.12.0: MIT (`licenses/yyjson-MIT.txt`). Whether each is linked into the shipped worker was not verified.
- **ptts 0.4.1** (`xn-ptts` rev `49b9033e`, linked into `speak`). Licence: MIT OR Apache-2.0. Texts: `licenses/ptts-MIT.txt` and `licenses/Apache-2.0.txt`. Obligation: licence copy for the chosen option.
- **flexaudio** (vendored, `crates/capture/vendor/flexaudio`). Licence: MIT, Copyright (c) 2026 tubome / Studio Sadola. Text: `licenses/flexaudio-MIT.txt`; upstream notes in `crates/capture/vendor/flexaudio/NOTICE.md`. Obligation: notice kept.
- **Vella Metal kernels** (`crates/listen/src/metal/vella_formats27.metal`; adapted tiles in `crates/decide/src/whisper.metal`, `crates/speak/src/pointwise.metal`, `crates/speakers/src/kernels.metal`, each crate with its `VELLA-LICENSE`; `crates/decide` is MIT-licensed for it). Licence: MIT, Copyright (c) 2026 Vella contributors. Text: `licenses/Vella-MIT.txt`. Obligation: notice kept.
- **cfg_aliases 0.2.2** (build-time crate, Rust registry). Licence: MIT. Its notice file is `licenses/cfg_aliases-NOTICES.txt`. Obligation: notice kept.

## Models

- **Parakeet ASR weights**: `FluidInference/parakeet-ultra-coreml` at `95eaa59a39d4394f047a4dc5cce480388a60d1b6`, a Core ML conversion of `moondream/parakeet-ultra`, a post-train of `nvidia/parakeet-tdt-0.6b-v3`. Licence: CC-BY-4.0. Text: `licenses/CC-BY-4.0.txt`. Credit: Parakeet Ultra by Moondream; parent by NVIDIA; Core ML conversion by FluidInference. Obligation: **attribution**. The conversion uses an int8 encoder and fp16 decoder/joint; the files are used as published. We did not confirm that the weights are unchanged in content, so that phrase is not used.
- **Silero VAD** (`silero_vad.onnx`, `snakers4/silero-vad` at `1e261b03`). Licence: MIT, Copyright (c) 2020-present Silero Team. Text: `licenses/Silero-VAD-MIT.txt`. Obligation: notice kept.
- **Smart Turn v3.2 CPU** (`pipecat-ai/smart-turn-v3` at `f766f81d`, stock model; the personal `toby-turn` variant is excluded from the public build). Licence: BSD-2-Clause, Copyright (c) 2024-2025 Daily. Text: `licenses/SmartTurn-BSD-2-Clause.txt`. Obligation: notice kept.
- **Phonon 31m English** (`gradium/phonon-31m-en` at `541f5752`) and **Phonon voice presets** (`gradium/phonon-voices` at `da6c9ec0`), by Gradium. Licence: Apache-2.0. Text: `licenses/Apache-2.0.txt`. Obligation: licence copy. The model card's responsible-use section (no impersonation, no cloning without consent) is a use term, not a licence condition.
- **Decider V6** (`d1/d1-2B-heimdall-v6-8bit`, published as https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6 `int8/`, a fine-tune of `LiquidAI/d1-2B` at `cfe5760f`). Downloaded by `scripts/fetch_decider.sh` and bundled in the app; never in this repository. Licence: LFM Open License v1.0, Copyright Liquid AI, Inc. Text: `licenses/LFM-Open-License-1.0.txt`. Obligation: **licence copy**, mark modified files, **notice kept**, and **terms**: commercial use is licensed only where the licensee's annual revenue is under USD 10,000,000 (the Threshold).
- **ReDimNet2-B6 speaker embedding** (`speakers/ReDimNet2B6.onnx`, from `soniqo/ReDimNet2-B6-ONNX-FP32`, upstream `PalabraAI/ReDimNet2`). Licence: MIT, Copyright (c) 2026 Palabra.ai. Text: `licenses/ReDimNet-MIT.txt`. Obligation: notice kept.
- **Nemotron 3 Diarization** (`speakers/Nemotron-3-Diarization.q8_0.gguf`, `nvidia/Nemotron-3-Diarization` at `f667ed73`). Licence: OpenMDW-1.1, copyright NVIDIA. Text: `licenses/OpenMDW-1.1.txt`, read from https://openmdw.ai/license/1-1/. Obligation: **licence copy** (the full agreement) and **notice kept**. The OpenMDW terms impose no use, field, revenue or output restriction.
- **MossFormer2 speech separation** (`night/model.int8.onnx`, `ipsilondev/MossFormer2-SS-16K-ONNX` at `abd8b126`; upstream `alibabasglab/MossFormer2_SS_16K`). Licence: Apache-2.0 for the ONNX export; MIT for the upstream code. Texts: `licenses/Apache-2.0.txt`, `licenses/MossFormer2-MIT.txt`. Obligation: licence copy, notice kept.
- **EmbeddingGemma 2** (`google/embeddinggemma-2` at `914f7f89`, memory-search embedding). Licence: Apache-2.0 (model card label; the Gemma 4 licence page states Apache License 2.0). Text: `licenses/EmbeddingGemma-Apache-2.0.txt`. Obligation: licence copy. The card says deployments must adhere to the Gemma Prohibited Use Policy: https://ai.google.dev/gemma/prohibited_use_policy (URL resolves, checked 2026-10-27).
- **Qwen3.8 27B UD-Q4_K_M GGUF** (`unsloth/Qwen3.8-27B-GGUF`, quantised from `Qwen/Qwen3.8-27B`). Licence: Apache-2.0, Copyright 2026 Alibaba Cloud. Text: `licenses/Qwen3.8-Apache-2.0.txt`. Heimdall downloads this model through Splash; it is not bundled. Obligation: show the licence and copyright in the download flow.

## Vendored source

- `crates/speakers/vendor/nemo-speech`: NVIDIA, Apache-2.0. Its own `NOTICE` and `THIRD_PARTY_NOTICES.md` are in that directory. The THIRD_PARTY file lists KenLM (LGPL), Open JTalk, CppJieba, SentencePiece, OpenFST, RE2, protobuf and CUDA. The speakers CMake build compiles only `src/asr/features`, `encoder`, `diar`, `common` and `runtime/ggml`, so those listed components are not in this build. That is from the CMake globs, not a release-build check. The THIRD_PARTY file also credits parakeet.cpp derived code (Jason Ni) without stating its licence. Open item.
- `crates/speakers/vendor/ggml`: MIT (see above).
- `crates/speakers/vendor/FluidAudio-LICENSE`: Apache-2.0, adapted algorithm (`licenses/FluidAudio-Apache-2.0.txt`). No FluidAudio code is shipped.
- `crates/speakers/vendor/ReDimNet-LICENSE`: MIT (see above).
- `crates/night/third-party`: MossFormer2 licences (see above).
- `crates/speak/phonon/NOTICE.md`: Heimdall's own notice for Phonon (Apache-2.0).
- `crates/decide/docs/LLAMA-CPP-LICENSE.txt`: llama.cpp MIT.

## Rust crates (summary)

- `Cargo.lock` at the audited commit: 679 packages. 658 come from crates.io; licences were read from the local registry cache. The git crate `ptts` is covered above.
- Declared licences among the 658 (count): MIT OR Apache-2.0 320; MIT 123; MIT/Apache-2.0 41; Apache-2.0 OR MIT 40; Zlib OR Apache-2.0 OR MIT 31; Unicode-3.0 18; Apache-2.0 13; BSD-3-Clause 8; Apache-2.0/MIT 6; Unlicense OR MIT 5; Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT 5; ISC 4; Unlicense 3; Zlib 3; CDLA-Permissive-2.0 2.
- Copyleft options: `attohttpc` (MPL-2.0) and `self_cell` (Apache-2.0 OR GPL-2.0-only) come from the iroh stack, which is personal-only. `mach-sys` (GPL-3.0 OR Apache-2.0) comes from `mlx-rs`; whether `mlx-rs` is in the public build was not traced.
- Obligation: MIT, BSD and ISC crates need their copyright notice shipped with the app. Apache-2.0 crates need their licence and any NOTICE. Only one registry crate has a NOTICE file (cfg_aliases, MIT). A generated per-crate notice bundle does not exist yet. See Open items.

## npm packages (summary)

- `crates/agent/runtime/package-lock.json`: 121 locked packages, all production (no dev or optional). The root package is `heimdall-pi-runtime` 1.0.0 (private).
- Declared licences: MIT 59, Apache-2.0 38, BSD-3-Clause 13, ISC 7, BlueOak-1.0.0 2, Unlicense 1, 0BSD 1.
- All 121 tarballs were downloaded from the lockfile URLs and matched their sha512 integrity. None contains a NOTICE file.
- Apache-2.0 packages (38 lock entries; obligation: `licenses/Apache-2.0.txt`, licence copy): `@aws-sdk/*` (21 entries, including two nested `@smithy/node-http-handler` copies), `@smithy/*` (6 top-level), `@aws/lambda-invoke-store`, `@google/genai`, `@silvia-odwyer/photon-node`, `ecdsa-sig-formatter`, `gaxios`, `gcp-metadata`, `google-auth-library`, `google-logging-utils`, `grok-mermaid`, `long`, `openai`.
- MIT, BSD, ISC and other packages: obligation is notice kept. Per-package notices are not yet copied into the app. See Open items.

## Not in the public build

- Drama 3 model and runtime, and Supertonic 3 assets: personal edition only (`scripts/bundle_models.py`, `scripts/check_public_bundle.py`).
- Smart Turn `toby-turn` variant: personal only.
- iroh peer-to-peer stack (`friends` crate): personal only.
- Xiph libopus (`crates/speakers/vendor/opus`, BSD-style): linked as a static library in the speakers CMake target, but `heimdall_nemo` has no undefined references to it, so no Opus code is pulled into the binary. Checked with `nm` on the existing debug build of `libheimdall_nemo.a`, not on a release build.
- Sortformer v2.1 Core ML weights (CC-BY-4.0, FluidInference) and Pyannote Community-1 QA export (CC-BY-4.0): QA only, not bundled.
- TCL intelligent_wakeup checkpoints: not wired in shipped source.

## Not a licence component

- **Wake cue**: the app plays macOS's system sound `Tink` by name (`NSSound.soundNamed`, `crates/app/src/bridge/speech_policy.rs`). No audio file is shipped and none appears in git history. Apple's system sound terms are not read.
- **Provider marks** (Anthropic, OpenAI/ChatGPT, OpenCode) shown in `crates/ui/assets/providers/`: third-party trademarks. No licence or permission is recorded in the repo. Kept as they are.

## Open items

- **Decider**: the public alpha bundles Decider V6 in 8-bit only; its LFM terms (licence copy, notice, USD 10M revenue threshold) pass on to anyone using it commercially.
- **Decider training data**: described on the model card at https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6.
- **Per-package notices for Rust crates and npm packages**: not generated. A build step should produce the full bundle from the lockfiles.
- **Unverified**: ONNX Runtime version; mlx C++ version inside mlx-swift 0.32.3; which Swift packages are linked into the worker; whether `mlx-rs` is in the public build; the parakeet.cpp licence stated in the NeMo tree; the full Node.js bundled-dependency texts beyond what is in `licenses/`.
