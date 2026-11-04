#!/usr/bin/env python3
"""Regenerate fixed DSP fixtures only, using Transformers 4.57.6 / numpy.
Run from a task-local Python environment; no neural model downloads or inference.
"""
import pathlib, wave
import numpy as np
from transformers import WhisperFeatureExtractor
ROOT = pathlib.Path(__file__).resolve().parents[1] / 'tests/fixtures'
features = WhisperFeatureExtractor(chunk_length=8)
def save(name, audio):
    audio = np.asarray(audio, dtype=np.float32)
    audio.astype('<f4').tofile(ROOT / f'features-{name}-audio.bin')
    tail = audio[-128000:]
    tail = np.pad(tail, (128000-len(tail), 0))
    result = features(tail, sampling_rate=16000, return_tensors='np',
                      padding='max_length', max_length=128000, truncation=True,
                      do_normalize=True)['input_features']
    result.astype('<f4').tofile(ROOT / f'features-{name}-expected.bin')
save('silence', np.zeros(128000, dtype=np.float32))
save('sine', np.sin(np.arange(16000, dtype=np.float32)*np.float32(2*np.pi*440/16000)))
save('boundary', np.arange(128001, dtype=np.float32)/128001)
with wave.open(str(ROOT/'en-2.wav')) as f:
    save('speech', np.frombuffer(f.readframes(f.getnframes()), dtype='<i2').astype(np.float32)/32768)
