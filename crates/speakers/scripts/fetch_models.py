#!/usr/bin/env python3
"""Agent-operated, bounded downloads. No auth or permission APIs. <= 200 MB assets."""
import hashlib,json,subprocess,urllib.request
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
OUT=ROOT/'models/speakers';OUT.mkdir(parents=True,exist_ok=True)
FILES=[
('Nemotron-3-Diarization.q8_0.gguf', 'https://huggingface.co/nvidia/Nemotron-3-Diarization/resolve/f667ed73aee57d40cc39428eb768b4fd87a0a29e/Nemotron-3-Diarization.q8_0.gguf', 107012128, '08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1'),
('ReDimNet2B6.onnx', 'https://huggingface.co/soniqo/ReDimNet2-B6-ONNX-FP32/resolve/e911e3f063899805f3d94ee5d1db53fff8e9f3e8/ReDimNet2B6.onnx', 51223453, 'd960e9f713eaba712f44498e82221b6fc12fcd7eaacd367c9eac45d3a6ce59ec'),
('ES2004a.Array1-01.wav', 'https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/ES2004a/audio/ES2004a.Array1-01.wav', 33579394, '6936edac5d0904fc5c4ab175546c5cc5366601fdc1b1e5183a6ea2c10f05d150'),
('ES2004a.rttm', 'https://raw.githubusercontent.com/pyannote/AMI-diarization-setup/67c2d539286e89f68952d5dcf83912bd9f01dfae/only_words/rttms/test/ES2004a.rttm', 14796, '9869c6146c2fd9595403edb36c2caeda65c12ffa2c0af4ce48d6814b673fd5a9'),
('ES2004a-forced.rttm', 'https://raw.githubusercontent.com/nttcslab-sp/diar-forced-alignment/9527b7c64846fb38316a610f32e9d3466bd6d8b7/AMI/test/ES2004a.rttm', 34230, '3ed314bb088e11a2a53f6eb1a8ed3876f6ae6f3f92d3340640baabac90167c98'),
('redimnet-config.json', 'https://huggingface.co/soniqo/ReDimNet2-B6-ONNX-FP32/resolve/e911e3f063899805f3d94ee5d1db53fff8e9f3e8/config.json', 1033, '703376f40fc66686fb6bf729aa504a964a955742202d449a95c9db9595c523cc'),
]
receipts=[]
for name,url,size,sha in FILES:
 p=OUT/name
 if not p.exists():
  temp=p.with_suffix(p.suffix+'.part')
  with urllib.request.urlopen(url,timeout=90) as r,temp.open('wb') as f:
   n=0
   while b:=r.read(1024*1024):
    n+=len(b)
    if n>150_000_000:raise RuntimeError('asset exceeds bounded size')
    f.write(b)
  temp.replace(p)
 actual=hashlib.sha256(p.read_bytes()).hexdigest()
 if size is not None and p.stat().st_size!=size:raise RuntimeError('size mismatch '+name)
 if sha is not None and actual!=sha:raise RuntimeError('digest mismatch '+name)
 receipts.append(dict(path=name,url=url,bytes=p.stat().st_size,sha256=actual));print(name,p.stat().st_size,flush=True)
(OUT/'downloads.json').write_text(json.dumps(receipts,indent=2)+'\n')
if '--verify-only' in __import__('sys').argv: raise SystemExit(0)
# First 480 s of the public four-speaker SDM meeting, mono, 16 kHz. No packet loss simulation.
subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-y','-i',str(OUT/'ES2004a.Array1-01.wav'),'-t','480','-ac','1','-ar','16000',str(OUT/'ami-480.wav')],check=True,timeout=30)
