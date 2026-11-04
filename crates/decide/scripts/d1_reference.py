"""Run LiquidAI's original PyTorch implementation against ORIGINAL reference F16 weights.

Reads GGUF into original module tensors (inverse official conversion). No 2.35-GB
FP32 checkpoint download; parity baseline is original F16, computed in torch fp32.
Batch-norm's folded affine is un-folded with mean=0,var=1-eps (exact equivalent).
"""
import hashlib,importlib.util,json,pathlib,re,sys
import numpy as np
import torch
from transformers import AutoTokenizer
CRATE=pathlib.Path(__file__).resolve().parents[1];ROOT=CRATE.parents[1];REF=CRATE/'ref';M=ROOT/'models'/'decide'
sys.path.insert(0,str(REF/'llama.cpp'/'gguf-py'))
import gguf

def load_reference():
 source=REF/'d1-omni-600M';(source/'__init__.py').touch()
 spec=importlib.util.spec_from_file_location('d1_original',source/'__init__.py',submodule_search_locations=[str(source)])
 pkg=importlib.util.module_from_spec(spec);sys.modules[spec.name]=pkg;spec.loader.exec_module(pkg)
 from d1_original.modeling_d1 import D1OmniConfig,D1OmniModel
 config=D1OmniConfig(**json.loads((source/'config.json').read_text()))
 torch.set_num_threads(2);torch.backends.mha.set_fastpath_enabled(False)
 model=D1OmniModel(config).eval()
 model.__dict__['tokenizer']=AutoTokenizer.from_pretrained(str(source),local_files_only=True,trust_remote_code=True)
 readers=[gguf.GGUFReader(str(M/'d1'/name)) for name in ['d1-omni-600M-F16.gguf','mmproj-d1-omni-600M-F16.gguf']]
 tensors={t.name:t for reader in readers for t in reader.tensors}
 maps=[gguf.get_tensor_name_map(gguf.MODEL_ARCH.LFM2,18),gguf.get_tensor_name_map(gguf.MODEL_ARCH.MMPROJ,17)]
 used=set();missing=[];state={}
 for original,target in model.state_dict().items():
  name=original
  media=name.startswith(('vision.','audio.'))
  if not media:
   name=name.replace('encoder.','model.',1) if name.startswith('encoder.') else name
   name=name.replace('head.head.layers.','head.layers.').replace('in_proj_','in_proj.')
   name=name.removeprefix('head.') if name.startswith(('head.type_emb','head.scorer')) else name
   if name.startswith('head.layers.'):
    parts=name.split('.');parts[2]=str(int(parts[2])+16);name='.'.join(parts)
  else:
   name=name.replace('vision.tower.','vision_tower.vision_model.').replace('vision.projector.','multi_modal_projector.').replace('audio.encoder.','conformer.')
   for old,new in [('adapter.norm',0),('adapter.linear_1',1),('adapter.linear_2',3),('residual.ln',4),('residual.down',5),('residual.up',6)]:name=name.replace(f'audio.{old}.',f'audio_adapter.model.{new}.')
  if 'batch_norm.running_mean' in original:state[original]=torch.zeros_like(target);continue
  if 'batch_norm.running_var' in original:state[original]=torch.full_like(target,1.-1e-5);continue
  if original.endswith('num_batches_tracked'):state[original]=torch.zeros_like(target);continue
  mapped=maps[int(media)].get_name(name,try_suffixes=('.weight','.bias'))
  if mapped not in tensors:
   missing.append((original,name,mapped));continue
  t=tensors[mapped];used.add(mapped);x=torch.from_numpy(np.array(t.data,copy=True).astype(np.float32))
  if 'vision_tower' in name and 'patch_embedding.weight' in name:
   x=x.permute(0,2,3,1).reshape(target.shape)
  else:x=x.reshape(target.shape)
  state[original]=x
 if missing:raise RuntimeError('unmapped tensors: '+str(missing))
 unused=set(tensors)-used
 if unused:raise RuntimeError('unused GGUF tensors: '+str(sorted(unused)))
 model.load_state_dict(state,strict=True);print('Reconstructed',len(state),'original tensors; GGUF tensors',len(used),flush=True)
 return model
if __name__=='__main__':
 m=load_reference()
 # Rust emits its exact fixed requests; evaluate the reference on those same inputs.
 import soundfile as sf
 from PIL import Image
 receipt=[]
 for line in open(sys.argv[1]):
  case=json.loads(line);q=case['request']['questions'];state=case['request']['state'];audio=None;image=None
  if 'audio' in case:audio=sf.read(CRATE/'tests'/'fixtures'/case['audio'],dtype='float32')[0]
  if 'image' in case:image=Image.open(CRATE/'tests'/'fixtures'/case['image'])
  out=m.system_one(state,q,images=image,audio=audio)
  request=dict(case['request']);files=request.pop('files',None)
  record=dict(name=case['name'],request=request,answers=out['answers'],usage=out['usage'],audio=case.get('audio'),image=case.get('image'))
  if files is not None:record['files_sha256']=hashlib.sha256(json.dumps(files,ensure_ascii=False,separators=(',',':')).encode()).hexdigest()
  receipt.append(record)
  print('reference',case['name'],out['answers'],flush=True)
 pathlib.Path(sys.argv[2]).write_text(json.dumps(receipt,indent=2)+'\n')
