#!/usr/bin/env python3
import pathlib,urllib.request,json,hashlib,concurrent.futures
root=pathlib.Path(__file__).resolve().parents[3]/'models/supertonic-3'
revision='3cadd1ee6394adea1bd021217a0e650ede09a323'
files=['onnx/'+x for x in ['duration_predictor.onnx','text_encoder.onnx','vector_estimator.onnx','vocoder.onnx','tts.json','unicode_indexer.json']]+['voice_styles/'+v+'.json' for v in ['M1','M2','M3','M4','M5','F1','F2','F3','F4','F5']]+['LICENSE']
def fetch(f):
 p=root/f;p.parent.mkdir(parents=True,exist_ok=True)
 url='https://huggingface.co/Supertone/supertonic-3/resolve/'+revision+'/'+f
 if not p.exists():
  tmp=p.with_suffix(p.suffix+'.partial')
  with urllib.request.urlopen(url,timeout=60) as response,tmp.open('wb') as out:
   while chunk:=response.read(1024*1024):out.write(chunk)
  tmp.replace(p)
 return {'file':f,'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'url':url}
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:assets=list(pool.map(fetch,files))
lock={'revision':revision,'assets':assets,'total_bytes':sum(x['bytes'] for x in assets)}
(pathlib.Path(__file__).parent/'models.lock.json').write_text(json.dumps(lock,indent=2)+'\n')
print('TTS model assets:',lock['total_bytes'],'bytes')
