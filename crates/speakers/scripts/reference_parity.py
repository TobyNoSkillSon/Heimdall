#!/usr/bin/env python3
"""Pinned NeMo reference capture on actual PCM, optionally same GGUF Q8 tensors.
Environment is disposable (ref/venv), output is ignored ref/. No runtime Python.
"""
import argparse,json,time,hashlib
from pathlib import Path
import numpy as np,soundfile as sf,torch
from nemo.collections.asr.models import SortformerEncLabelModel
ROOT=Path(__file__).resolve().parents[3]
p=argparse.ArgumentParser();p.add_argument('--checkpoint',type=Path,required=True);p.add_argument('--wav',type=Path,required=True);p.add_argument('--out',type=Path,required=True);p.add_argument('--seconds',type=int,default=480);p.add_argument('--gguf',type=Path);p.add_argument('--threads',type=int,default=1);a=p.parse_args();a.out.mkdir(parents=True,exist_ok=True)
torch.set_num_threads(a.threads)
m=SortformerEncLabelModel.restore_from(str(a.checkpoint),map_location='cpu').eval()
replaced=[]
if a.gguf:
 from gguf import GGUFReader,dequantize
 reader=GGUFReader(str(a.gguf));tensors={t.name:t for t in reader.tensors};state=m.state_dict()
 for key,value in state.items():
  name=key.replace('sortformer_modules.encoder_proj.','encoder_proj.').replace('sortformer_modules.subpixel_upsample.','subpixel_upsample.')
  if name=='sortformer_modules.learnable_sil_emb':name='learnable_sil_emb'
  elif name.startswith('sortformer_modules.'):name=name.replace('sortformer_modules.','head.',1)
  if key=='preprocessor.featurizer.fb':name='preprocessor.fb'
  if name in tensors:
   t=tensors[name];array=dequantize(t.data,t.tensor_type).reshape(tuple(t.shape[::-1]));assert array.size==value.numel(),key
   value.copy_(torch.from_numpy(array.copy()).reshape(value.shape));replaced.append(key)
 m.load_state_dict(state)
s=m.sortformer_modules
s.chunk_len=3;s.chunk_right_context=1;s.chunk_left_context=0;s.fifo_len=264;s.spkcache_len=264;s.spkcache_update_period=222;m._check_streaming_parameters()
x,sr=sf.read(a.wav,dtype='float32');assert sr==16000 and x.ndim==1;x=x[:a.seconds*sr]
with torch.inference_mode():
 features,length=m.process_signal(torch.tensor(x)[None],torch.tensor([len(x)]));np.save(a.out/'features.npy',features[0].T.numpy())
 started=time.monotonic();probs=m.forward_streaming(features,length)[0].numpy();np.save(a.out/'probabilities.npy',probs)
 segments=[]
 for speaker in range(8):
  active=probs[:len(x)//160,speaker]>=.5;edges=np.diff(np.r_[False,active,False].astype(int));starts=np.where(edges==1)[0];ends=np.where(edges==-1)[0]
  segments.extend(dict(start_time=float(start/100),end_time=float(end/100),speaker=speaker+1) for start,end in zip(starts,ends))
 receipt={'processing_seconds':time.monotonic()-started,'dtype':'float32 CPU','torch':torch.__version__,'nemo':__import__('nemo').__version__,'checkpoint_sha256':hashlib.sha256(a.checkpoint.read_bytes()).hexdigest(),'gguf_sha256':hashlib.sha256(a.gguf.read_bytes()).hexdigest() if a.gguf else None,'replaced_tensors':replaced,'wav_sha256':hashlib.sha256(a.wav.read_bytes()).hexdigest(),'feature_shape':list(features.shape),'valid_frames':length.tolist(),'probability_shape':list(probs.shape),'segments':segments}
 (a.out/'receipt.json').write_text(json.dumps(receipt,indent=2))
 print('reference complete',probs.shape,flush=True)
