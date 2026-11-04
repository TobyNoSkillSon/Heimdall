#!/usr/bin/env python3
"""Capture small fixed tensors/profile once, using the pinned CPU model files.
Development dependencies: numpy, onnxruntime. CPU outputs are subsequently
re-captured by heimdall-tts-module (the actual Rust/ort path) for parity gates.
"""
import collections, json, pathlib, hashlib
import numpy as np
import onnxruntime as ort

root=pathlib.Path(__file__).resolve().parents[3]
out=root/'models/reference/shader-speak';out.mkdir(parents=True,exist_ok=True)
style=json.loads((root/'models/supertonic-3/voice_styles/M1.json').read_text())
ids=np.arange(1,11,dtype=np.int64)[None,:];mask=np.ones((1,1,10),np.float32)
emb=np.random.default_rng(42).normal(size=(1,256,10)).astype(np.float32)
latent=np.random.default_rng(43).normal(size=(1,144,100)).astype(np.float32)
ttl=np.array(style['style_ttl']['data'],np.float32)
feeds={
 'duration_predictor':dict(text_ids=ids,text_mask=mask,style_dp=np.array(style['style_dp']['data'],np.float32)),
 'text_encoder':dict(text_ids=ids,text_mask=mask,style_ttl=ttl),
 'vector_estimator':dict(noisy_latent=latent,text_emb=emb,style_ttl=ttl,latent_mask=np.ones((1,1,100),np.float32),text_mask=mask,current_step=np.array([0],np.float32),total_step=np.array([5],np.float32)),
 'vocoder':dict(latent=latent),
}
receipt={'onnxruntime':ort.__version__,'numpy':np.__version__,'modules':{}}
for name,feed in feeds.items():
 manifest=out/(name+'-inputs.json')
 if manifest.exists():raise RuntimeError(f'reference already captured: {manifest}; preserve/reuse it')
 options=ort.SessionOptions();options.intra_op_num_threads=2;options.inter_op_num_threads=1;options.enable_profiling=True;options.profile_file_prefix=str(out/name)
 for k in ['session.intra_op.allow_spinning','session.inter_op.allow_spinning']:options.add_session_config_entry(k,'0')
 session=ort.InferenceSession(str(root/'models/supertonic-3/onnx'/(name+'.onnx')),options,providers=['CPUExecutionProvider'])
 session.run(None,feed);values=session.run(None,feed);np.savez(out/(name+'-outputs.npz'),**dict(zip([x.name for x in session.get_outputs()],values)))
 profile=session.end_profiling();cost=collections.Counter()
 for event in json.loads(pathlib.Path(profile).read_text()):
  if event.get('cat')=='Node':cost[event.get('args',{}).get('op_name','?')]+=event['dur']
 specs=[]
 for key,value in feed.items():
  path=out/(name+'-'+key+'.bin');path.write_bytes(value.ownertes());specs.append(dict(name=key,shape=list(value.shape),dtype='i64' if value.dtype==np.int64 else 'f32',path=str(path),sha256=hashlib.sha256(path.read_bytes()).hexdigest()))
 manifest.write_text(json.dumps(specs,indent=2));receipt['modules'][name]={'profile':profile,'total_node_us':sum(cost.values()),'op_us':dict(cost)}
(out/'capture.json').write_text(json.dumps(receipt,indent=2));print(json.dumps(receipt))
