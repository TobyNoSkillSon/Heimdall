"""Offline, resident EmbeddingGemma 2 worker; stdout is an ID-matched JSON protocol.

The benchmark imports this same runtime. Serving uses pinned BF16/FP16 weights,
guarded trunk kernels, and no remote code, network or quantization.
"""
import ctypes
import json
import os
from pathlib import Path
import subprocess
import sys
import time

os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

import numpy as np

MODEL_ID = "google/embeddinggemma-2"
REVISION = "914f7f89142e33e77833254d9c9b90c3cef7303b"
PRECISION = "bf16"
VERSION = REVISION + ":mlx-vlm-3d87e884:bf16:audio-cast-v1:input-v1:trunk-b1"
DIMENSIONS = 768


def optimize_trunk(model, mx):
    """Loaded, immutable BF16/FP16 trunk only; media encoders stay upstream.

    Tensor-op down projections are shape/hardware guarded. Rotary tables live for
    one forward, never across requests. The environment switches are lab controls.
    """
    import mlx.nn as nn
    import platform
    info = mx.device_info()
    architecture = info.get("architecture", "")
    import re
    match = re.fullmatch(r"applegpu_g(\d+)([a-z])", architecture)
    os_version = tuple(int(x) for x in platform.mac_ver()[0].split(".")[:2])
    tensor_ops = bool(match and int(match[1]) >= (18 if match[2] == "p" else 17)
                      and os_version >= (26, 2))
    tensor_ops = tensor_ops and os.environ.get("HEIMDALL_EMBED_SMALLM", "1") != "0"
    kernel = None
    if tensor_ops:
        # Dense tile-1 adapted from Vella/Worker/Sources/SmallMGEMM. K split in
        # four changes FP32 accumulation order, retaining the BF16/FP16 store.
        kernel = mx.fast.metal_kernel(name="heimdall_eg2_down", input_names=["x", "w"], output_names=["out"],
            header="""
 #include <metal_tensor>
 #include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>
 using namespace mpp::tensor_ops;
 """, source="""
 const int M = x_shape[0], K = x_shape[1], N = w_shape[0];
 const int tm = threadgroup_position_in_grid.x, tn = threadgroup_position_in_grid.y;
 const int sg = simdgroup_index_in_threadgroup, kc = K / 4, k0 = sg * kc;
 auto A = tensor<device T, dextents<int32_t, 2>, tensor_inline>((device T*)x + k0, dextents<int32_t, 2>(kc, M), array<int, 2>{1, K});
 auto B = tensor<device T, dextents<int32_t, 2>, tensor_inline>((device T*)w + k0, dextents<int32_t, 2>(kc, N), array<int, 2>{1, K});
 constexpr auto desc = matmul2d_descriptor(32, 32, static_cast<int>(dynamic_extent), false, true, false);
 matmul2d<desc, execution_simdgroup> op;
 auto mA = A.slice(0, tm * 32), mB = B.slice(0, tn * 32);
 auto cT = op.template get_destination_cooperative_tensor<decltype(mA), decltype(mB), float>();
 for (uint16_t i = 0; i < cT.get_capacity(); ++i) { if (cT.is_valid_element(i)) cT[i] = 0; }
 op.run(mA, mB, cT);
 threadgroup float red[4][1024];
 for (uint16_t i = 0; i < cT.get_capacity(); ++i) {
     if (!cT.is_valid_element(i)) continue;
     auto idx = cT.get_multidimensional_index(i);
     red[sg][idx[1] * 32 + idx[0]] = cT[i];
 }
 threadgroup_barrier(mem_flags::mem_threadgroup);
 for (int e = thread_index_in_threadgroup; e < 1024; e += 128) {
     float s = 0.0f;
     for (int j = 0; j < 4; ++j) s += red[j][e];
     const int n = tn * 32 + e % 32, m = tm * 32 + e / 32;
     if (n < N && m < M) out[m * N + n] = static_cast<T>(s);
 }
 """)

    class DownProjection(nn.Module):
        def __init__(self, original):
            super().__init__()
            self.original = original
        def __call__(self, x):
            w = self.original.weight
            rows = int(np.prod(x.shape[:-1]))
            if (kernel is not None and 9 <= rows <= 768 and w.shape == (512, 2048)
                    and x.shape[-1] == 2048 and x.dtype == w.dtype
                    and x.dtype in (mx.bfloat16, mx.float16)
                    and "bias" not in self.original):
                return kernel(inputs=[x.reshape(rows, 2048), w], template=[("T", x.dtype)],
                    grid=(((rows+31)//32)*128, 16, 1), threadgroup=(128,1,1),
                    output_shapes=[(rows,512)], output_dtypes=[x.dtype])[0].reshape(*x.shape[:-1],512)
            return self.original(x)

    class Rotary(nn.Module):
        def __init__(self, original):
            super().__init__()
            self.original = original
            self._tables = {}
        def __call__(self, x, positions):
            key = x.dtype
            if key not in self._tables:
                angles = positions.astype(mx.float32)[...,None] * self.original._frequencies
                angles = mx.concatenate([angles,angles],axis=-1)[:,None]
                self._tables[key] = (mx.cos(angles).astype(x.dtype), mx.sin(angles).astype(x.dtype))
            cos,sin = self._tables[key]
            half_width = x.shape[-1]//2
            rotated = mx.concatenate([-x[...,half_width:], x[...,:half_width]],axis=-1)
            return x*cos + rotated*sin

    trunk = model.language_model
    ropes = {}
    for i, layer in enumerate(trunk.layers):
        if tensor_ops:
            layer.mlp.down_proj = DownProjection(layer.mlp.down_proj)
            # The four global layers also have a 2048 -> 512 output projection.
            if layer.self_attn.o_proj.weight.shape == (512,2048):
                layer.self_attn.o_proj = DownProjection(layer.self_attn.o_proj)
        if os.environ.get("HEIMDALL_EMBED_ROPE", "1") != "0":
            key = (layer.self_attn.head_dim, trunk.config.layer_types[i])
            if key not in ropes:
                ropes[key] = Rotary(layer.self_attn.rope)
            layer.self_attn.rope = ropes[key]

    class Trunk(nn.Module):
        def __init__(self, original):
            super().__init__()
            self.original = original
        @property
        def embed_tokens(self):
            return self.original.embed_tokens
        @property
        def layers(self):
            return self.original.layers
        @property
        def config(self):
            return self.original.config
        def __call__(self, x, mask, positions=None):
            # A request-local cache avoids stale positional constants and retains
            # reference FP32 angles and separate activation-dtype multiplies.
            for rope in ropes.values():
                rope._tables.clear()
            try:
                return self.original(x,mask,positions)
            finally:
                for rope in ropes.values():
                    rope._tables.clear()
    model.language_model = Trunk(trunk)


def usage(pid=None):
    """macOS rusage_info_v6, oversized so a future ABI cannot overwrite Python's heap."""
    if sys.platform != "darwin":
        return {}
    raw = (ctypes.c_uint64 * 128)()
    lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    energy_available = not lib.proc_pid_rusage(pid or os.getpid(), 6, ctypes.byref(raw))
    if not energy_available and lib.proc_pid_rusage(pid or os.getpid(), 4, ctypes.byref(raw)):
        raise RuntimeError("process footprint counters unavailable")
    # Two uint64 slots are ri_uuid, followed by the SDK's v6 field order.
    timebase = (ctypes.c_uint32 * 2)()
    lib.mach_timebase_info(ctypes.byref(timebase))
    return {"cpu_s": (raw[2] + raw[3]) * timebase[0] / timebase[1] / 1e9,
            "footprint_bytes": raw[9], "peak_footprint_bytes": raw[30],
            "energy_j": raw[42] / 1e9 if energy_available else None}


SWAP_WINDOW_S = 120.0


def memory_state():
    vm = subprocess.run(["/usr/bin/vm_stat"], capture_output=True, text=True, timeout=2, check=True).stdout
    counts = {line.split(":")[0]: int(line.split(":")[1].strip().rstrip("."))
              for line in vm.splitlines()[1:] if ":" in line}
    page_size = int(vm.split("page size of ")[1].split()[0])
    # vm_stat's free excludes speculative; unlike host_statistics64, add it here. Purgeable pages and the file-backed
    # share of inactive pages are reclaimable without swapping (as in the native worker); inactive anonymous memory is
    # not. Without the file-backed count, inactive pages are not counted at all.
    file_inactive = min(counts["Pages inactive"], counts.get("File-backed pages", 0))
    free = (counts["Pages free"] + counts["Pages speculative"] + counts["Pages purgeable"] + file_inactive) * page_size
    swap = subprocess.run(["/usr/sbin/sysctl", "-n", "vm.swapusage"], capture_output=True,
                          text=True, timeout=2, check=True).stdout
    return free, float(swap.split("used = ")[1].split("M")[0]) * 1024**2


def install_audio_frontend(processor, mx):
    """Keep official padding/masks; move long-clip spectral work to MLX FP32.

    Short clips keep NumPy: the GPU submission/readback costs more than their
    FFT. The pinned checkpoint uses no dither/preemphasis/bin normalization.
    Unknown extractor configurations retain their own upstream implementation.
    """
    f = processor.feature_extractor
    if (f.dither != 0 or f.input_scale_factor != 1 or f.preemphasis != 0
            or f.per_bin_mean is not None or f.per_bin_stddev is not None
            or (f.frame_length, f.hop_length, f.fft_length) != (320, 160, 512)):
        return
    original = f._extract_spectrogram
    window = mx.array(f.window, dtype=mx.float32)
    filters = mx.array(f.mel_filters, dtype=mx.float32)

    @mx.compile
    def spectral(waveform):
        if waveform.ndim == 1:
            waveform = mx.expand_dims(waveform, 0)
        waveform = mx.pad(waveform, [(0, 0), (f.frame_length // 2, 0)])
        # Upstream unfolds 321 samples and then discards the final one. Keep
        # that count even though the FFT consumes only 320 samples per frame.
        count = (waveform.shape[-1] - f.frame_length - 1) // f.hop_length + 1
        frames = mx.as_strided(waveform, shape=(waveform.shape[0], count, f.frame_length),
                               strides=(waveform.shape[-1], f.hop_length, 1))
        magnitude = mx.abs(mx.fft.rfft(frames * window, n=f.fft_length, axis=-1))
        return mx.log(magnitude @ filters + float(f.mel_floor)).squeeze(0)

    def extract(waveform, attention_mask):
        # 800 frames (~8 seconds) is the conservative measured crossover:
        # 335-frame requests had no repeatable benefit, 840+ frames did.
        count = (waveform.shape[-1] + f.frame_length // 2 - f.frame_length - 1) // f.hop_length + 1
        if count < 800:
            return original(waveform, attention_mask)
        features = spectral(mx.array(waveform, dtype=mx.float32))
        mx.eval(features)
        mask = np.pad(attention_mask, (f.frame_length // 2, 0), constant_values=0)
        ends = np.arange(features.shape[0]) * f.hop_length + f.frame_length
        return np.asarray(features), mask[ends].astype(bool)

    f._extract_spectrogram = extract


class Runtime:
    def __init__(self, weights, backend="mlx", precision=PRECISION):
        from transformers import AutoProcessor
        self.weights = Path(weights)
        self.backend = backend
        if precision not in ("bf16","fp16"):
            raise ValueError("served precision must be BF16 or FP16; FP32 is reference-only")
        self.precision = precision if backend=="mlx" else "float32"
        free, swap = memory_state()
        self.swap_samples = [(time.monotonic(), swap)]
        self.footprint_limit = min((12 if backend=="reference" else 8) * 1024**3, free // 4)
        if free < 8 * 1024**3:
            raise RuntimeError("embedding admission needs 8 GiB free, no swap growth allowed")
        self.processor = AutoProcessor.from_pretrained(self.weights, local_files_only=True, use_fast=False)
        if backend == "mlx":
            import mlx.core as mx
            from mlx_vlm.embedding_loader import load_embedding_model
            self.mx = mx
            mx.set_memory_limit(min(6 * 1024**3, free // 4))
            mx.set_cache_limit(128 * 1024**2)
            # Same patch geometry and token counts; pinned torchvision CPU
            # bicubic avoids the PIL rescale/patchify copies. GPU resizing was
            # slower after idle, so vision remains on the CPU.
            from transformers import AutoImageProcessor
            self.processor.image_processor = AutoImageProcessor.from_pretrained(
                self.weights, local_files_only=True, backend="torchvision")
            install_audio_frontend(self.processor, mx)
            self.model = load_embedding_model(self.weights)
            # Production always uses half precision. FP32 exists only in the
            # independent Transformers comparison and can never be selected here.
            self.model.set_dtype(mx.bfloat16 if precision=="bf16" else mx.float16)
            mx.eval(self.model.parameters())
            mx.clear_cache()
            # The upstream Gemma4 audio attention computes context in FP32, but
            # Transformers casts it back to the input activation dtype BEFORE
            # the output projection. Without that boundary MLX promotes the
            # rest of all twelve Conformer blocks to FP32 under a BF16 policy.
            # This is reference arithmetic, not a lossy or optimized fast path.
            import mlx.nn as nn
            class AudioPostProjection(nn.Module):
                def __init__(self, projection):
                    super().__init__()
                    self.projection = projection
                def __call__(self, x):
                    return self.projection(x.astype(self.projection.linear.weight.dtype))
            for layer in self.model.audio_tower.layers:
                layer.self_attn.post = AudioPostProjection(layer.self_attn.post)
            if os.environ.get("HEIMDALL_EMBED_TRUNK", "1") != "0":
                optimize_trunk(self.model, mx)
        elif backend == "reference":
            import torch
            from transformers import AutoModel
            self.torch = torch
            torch.set_num_threads(4)
            # Independent Transformers execution. MPS has native BF16 on this Mac;
            # no fallback is allowed. CPU remains available for saved triangulation.
            self.reference_device = os.environ.get("HEIMDALL_EMBED_REFERENCE_DEVICE", "mps")
            if self.reference_device=="mps":
                torch.mps.set_per_process_memory_fraction(min(6*1024**3,free//4)/torch.mps.recommended_max_memory())
            self.model = AutoModel.from_pretrained(self.weights, local_files_only=True,
                dtype=torch.float32).eval().to(self.reference_device)
        else:
            raise ValueError("unknown runtime")

    def admit(self):
        if time.monotonic() - getattr(self, "last_admission", 0) < 0.5:
            return
        self.last_admission = time.monotonic()
        free, swap = memory_state()
        # Growth above the lowest swap of the last two minutes pauses: a steady creep trips it, while swap another
        # process added once pauses the index for at most the window (as in the native worker).
        now = time.monotonic()
        self.swap_samples = [s for s in self.swap_samples if now - s[0] <= SWAP_WINDOW_S] + [(now, swap)]
        growing = swap > min(s[1] for s in self.swap_samples) + 16 * 1024**2
        if free < 4 * 1024**3 or growing:
            raise RuntimeError("embedding paused: low free memory or swap growth")
        if usage().get("footprint_bytes", 0) > self.footprint_limit:
            raise RuntimeError("embedding process exceeded its bounded footprint")

    def prepare(self, item):
        from PIL import Image
        prefix = "task: search result | query: " if item.get("query") else "title: none | text: "
        prompt = prefix
        images, audios, videos = [], [], []
        for part in item["parts"]:
            kind = part["type"]
            if kind == "text":
                # Plain archived text must never create fake media placeholders.
                if any(x in part["text"] for x in ["<|image|>", "<|audio|>", "<|video|>"]):
                    raise ValueError("literal model media token in text")
                prompt += part["text"]
            elif kind == "image":
                with Image.open(part["path"]) as image:
                    images.append(image.convert("RGB"))
                prompt += "<|image|>"
            elif kind == "audio":
                audio = np.fromfile(part["path"], dtype="<f4")
                if not len(audio) or not np.isfinite(audio).all():
                    raise ValueError("invalid mono 16 kHz audio")
                audios.append(audio)
                prompt += "<|audio|>"
            elif kind == "video":
                frames = []
                for path in part["frames"]:
                    with Image.open(path) as image:
                        frames.append(np.asarray(image.convert("RGB")))
                videos.append(np.stack(frames))
                prompt += "<|video|>"
            else:
                raise ValueError("unsupported input part")
        kwargs = {"text": [prompt], "return_tensors": "np", "padding": True,
                  "truncation": False}
        if images:
            kwargs["images"] = [images]
        if audios:
            kwargs.update(audio=audios, sampling_rate=16000)
        if videos:
            # Already sampled by the recorder; do not resample or infer an FPS.
            kwargs.update(videos=videos, do_sample_frames=False, add_timestamps=False)
        inputs = self.processor(**kwargs)
        if inputs["input_ids"].shape[-1] > 8192:
            raise ValueError("input exceeds 8192 tokens; split at the archive boundary")
        return {key: value for key, value in inputs.items()
                if isinstance(value, np.ndarray)}

    def forward(self, inputs):
        if self.backend == "mlx":
            mx = self.mx
            values = {k: mx.array(v) for k, v in inputs.items()}
            if "input_features" in values:
                values["input_features"] = values["input_features"].astype(self.model.audio_tower.subsample_conv_projection.layer0.conv.weight.dtype)
            # Vision scales FP32 pixels BEFORE casting at its projection boundary.
            out = self.model(**values).text_embeds
            mx.eval(out)
            return np.asarray(out.astype(mx.float32))[0]
        torch = self.torch
        values = {k: torch.from_numpy(v.copy()).to(self.reference_device) for k, v in inputs.items()}
        with torch.inference_mode():
            hidden = self.model(**values).last_hidden_state
            # Sentence Transformers' published mean-pool includes the prompt.
            mask = values["attention_mask"].unsqueeze(-1).expand(hidden.size()).float()
            out = (hidden * mask).sum(1) / mask.sum(1).clamp(min=1e-9)
            out = torch.nn.functional.normalize(out, p=2, dim=1)
        vector=out.float().cpu().numpy()[0]
        if self.reference_device=="mps":
            # Reference-only scratch, not model weights. Long archived finals have
            # many different context shapes; do not retain every MPS allocation.
            del hidden,out,values,mask
            torch.mps.empty_cache()
        return vector

    def windows(self, item):
        """Context overflow only: preserve all text/media, with no silent truncation.

        Native audio is bounded to two minutes by Rust. Token offsets slice original
        text rather than decoding/rewording it. Oversized clips/documents use a
        token-weighted mean of normalized window vectors, then normalize once.
        Ordinary inputs keep their original, fused path exactly.
        """
        parts = []
        for part in item["parts"]:
            if part["type"] == "text":
                text = part["text"]
                encoded = self.processor.tokenizer(text, add_special_tokens=False, return_offsets_mapping=True)
                offsets = encoded["offset_mapping"]
                if len(offsets) <= 1024:
                    parts.append((part, len(offsets)))
                else:
                    start = 0
                    for i in range(0, len(offsets), 1024):
                        end = offsets[min(i + 1024, len(offsets)) - 1][1]
                        if i + 1024 >= len(offsets):
                            end = len(text)
                        parts.append(({"type": "text", "text": text[start:end]}, min(1024, len(offsets) - i)))
                        start = end
            elif part["type"] == "audio":
                parts.append((part, int(np.ceil(Path(part["path"]).stat().st_size / 4 / 640)) + 2))
            elif part["type"] == "image":
                parts.append((part, 282))
            elif part["type"] == "video":
                for i in range(0, len(part["frames"]), 20):
                    frames = part["frames"][i:i+20]
                    parts.append(({"type": "video", "frames": frames}, 142 * len(frames)))
        windows, current, tokens = [], [], 0
        for part, cost in parts:
            if current and tokens + cost > 6000:
                windows.append({**item, "parts": current})
                current, tokens = [], 0
            if cost > 6000:
                raise ValueError("single media part exceeds bounded context; split at the archive reader")
            current.append(part);tokens += cost
        if current:
            windows.append({**item, "parts": current})
        return windows

    def embed(self, items, dimensions=DIMENSIONS):
        if dimensions not in (128, 256, 512, 768) or not items:
            raise ValueError("invalid embedding geometry")
        self.admit()
        start = time.perf_counter()
        vectors = []
        tokens = 0
        window_count = 0
        for item in items:
            try:
                prepared = [self.prepare(item)]
            except ValueError as error:
                if "input exceeds 8192 tokens" not in str(error):
                    raise
                prepared = [self.prepare(window) for window in self.windows(item)]
            lengths = [p["input_ids"].shape[-1] for p in prepared]
            tokens += sum(lengths);window_count += len(prepared)
            for inputs, length in zip(prepared, lengths):
                self.admit()
                vector = self.forward(inputs)
                vectors.append(vector * (item.get("weight", 1.0) * length / sum(lengths)))
        vector = np.sum(vectors, axis=0, dtype=np.float32)[:dimensions]
        norm = np.linalg.norm(vector)
        if not np.isfinite(vector).all() or not norm > 0:
            raise ValueError("nonfinite or zero embedding")
        vector /= norm
        self.admit()
        return vector.tolist(), {"seconds": time.perf_counter() - start,
                                 "tokens": tokens, "windows": window_count}


def serve(weights):
    runtime = Runtime(weights)
    for line in sys.stdin:
        request = {}
        try:
            if len(line) > 1024**2:
                raise ValueError("request exceeds protocol bound")
            request = json.loads(line)
            vector, stats = runtime.embed(request["inputs"], request.get("dimensions", DIMENSIONS))
            reply = {"id": request["id"], "vector": vector, "stats": stats,
                     "model_id": MODEL_ID, "model_version": VERSION}
        except Exception as error:
            reply = {"id": request.get("id"), "error": str(error)}
        print(json.dumps(reply, allow_nan=False), flush=True)


if __name__ == "__main__":
    serve(sys.argv[1])
