"""Agent-only reference/export setup, pinned upstream source; no runtime Python."""
import concurrent.futures, hashlib, json, pathlib, urllib.request
CRATE = pathlib.Path(__file__).resolve().parents[1]
REF = CRATE / 'ref'
MODEL = CRATE.parents[1] / 'models' / 'decide'
SOURCES = {
 'LiquidAI/d1-omni-600M': '02b55d7076f15129e59ab3f94783f32c4b088674',
}
def get(repo_rev):
 repo,rev=repo_rev; path=REF/repo.split('/')[-1]
 meta=json.load(urllib.request.urlopen(f'https://huggingface.co/api/models/{repo}/revision/{rev}',timeout=30))
 receipt=[]
 for item in meta['siblings']:
  name=item['rfilename']
  if not (name.endswith(('.py','.json','.yaml','.txt')) or name=='LICENSE') or name=='.gitattributes':continue
  p=path/name;p.parent.mkdir(parents=True,exist_ok=True)
  url=f'https://huggingface.co/{repo}/resolve/{rev}/{name}'
  data=urllib.request.urlopen(url,timeout=30).read();p.write_bytes(data)
  receipt.append(dict(repo=repo,revision=rev,path=str(p.relative_to(CRATE.parents[1])),sha256=hashlib.sha256(data).hexdigest(),bytes=len(data),url=url))
 return receipt
if __name__=='__main__':
 receipts=[]
 for result in concurrent.futures.ThreadPoolExecutor(3).map(get,SOURCES.items()):receipts.extend(result)
 (CRATE/'scripts'/'sources.lock.json').write_text(json.dumps(receipts,indent=2)+'\n')
 print('Pinned reference sources:',len(receipts))
