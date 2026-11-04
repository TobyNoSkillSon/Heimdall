---
language:
- en
license: apache-2.0
tags:
- speech-separation
- audio
- onnx
- int8
- mossformer2
- clearvoice
pipeline_tag: audio-to-audio
---

# MossFormer2_SS_16K (ONNX INT8)

Quantized INT8 ONNX export of **MossFormer2_SS_16K**, a 16 kHz time-domain monaural speech separation model developed by Alibaba Group within the [ClearerVoice-Studio](https://github.com/modelscope/ClearerVoice-Studio) project.

- **Upstream Checkpoint**: [alibabasglab/MossFormer2_SS_16K](https://huggingface.co/alibabasglab/MossFormer2_SS_16K)
- **Model Size**: 90.9 MB (60% reduction from 230 MB FP32 ONNX / 670 MB PyTorch checkpoint)
- **Format**: ONNX INT8 (Opset 13)

---

## Model Architecture & I/O

- **Task**: 16 kHz Monaural Speech Separation (2 speakers).
- **Input**:
  - `inputs`: `[N, sequence_length]` (float32, 16 kHz raw waveform).
- **Outputs**:
  - `spk0`: `[N, sequence_length]` (float32, separated Speaker 1).
  - `spk1`: `[N, sequence_length]` (float32, separated Speaker 2).

---

## Verification & Benchmark

Tested against the official Alibaba PyTorch FP32 baseline on `input_ss.wav` (4.41s mixed 2-speaker audio):

| Metric | Speaker 0 | Speaker 1 | Status |
|---|---|---|---|
| **Cosine Similarity** | **0.999992** | **0.999986** | Near bit-exact |
| **Pearson Correlation ($r$)** | **0.999992** | **0.999986** | Near bit-exact |
| **SI-SNR** | **47.76 dB** | **45.58 dB** | Inaudible loss (>45 dB) |
| **RMSE** | **0.0234** | **0.0235** | High fidelity |

---

## Usage in Python

```python
import numpy as np
import soundfile as sf
import onnxruntime as ort

# 1. Load audio at 16kHz
audio, sr = sf.read("mixed_16k.wav")
assert sr == 16000
audio_in = audio[np.newaxis, :].astype(np.float32)

# 2. Run ONNX Session
sess = ort.InferenceSession("model.int8.onnx", providers=["CPUExecutionProvider"])
spk0, spk1 = sess.run(None, {"inputs": audio_in})

# 3. RMS Renormalization
rms_input = np.sqrt(np.mean(audio ** 2))
spk0_norm = (spk0[0] / (np.sqrt(np.mean(spk0[0]**2)) + 1e-12)) * rms_input
spk1_norm = (spk1[0] / (np.sqrt(np.mean(spk1[0]**2)) + 1e-12)) * rms_input

# 4. Save separated streams
sf.write("speaker0.wav", spk0_norm, 16000)
sf.write("speaker1.wav", spk1_norm, 16000)
```

---

## License

Apache-2.0
