"""Agent-only downloader. Immutable revisions; rejects missing/mismatched checksums."""
import concurrent.futures, hashlib, json, os, pathlib, urllib.request
ROOT = pathlib.Path(__file__).resolve().parents[3] / 'models' / 'decide'
LOCK = pathlib.Path(__file__).with_name('models.lock.json')
def fetch(x):
    p = ROOT / x['path']; p.parent.mkdir(parents=True,exist_ok=True)
    def valid():
        return p.exists() and p.stat().st_size == x['bytes'] and hashlib.file_digest(p.open('rb'),'sha256').hexdigest() == x['sha256']
    if valid(): return x['path'] + ' verified'
    if 'url' not in x:
        raise RuntimeError('Missing or changed local fine-tune: '+x['path']+'; restore from '+x['provenance'])
    tmp=p.with_suffix(p.suffix+'.part')
    with urllib.request.urlopen(x['url'],timeout=120) as r, tmp.open('wb') as f:
        while data:=r.read(1024*1024): f.write(data)
    if tmp.stat().st_size != x['bytes'] or hashlib.file_digest(tmp.open('rb'),'sha256').hexdigest()!=x['sha256']:
        tmp.unlink(); raise RuntimeError('checksum mismatch: '+x['path'])
    os.replace(tmp,p);return x['path']+' downloaded'
if __name__=='__main__':
    for msg in concurrent.futures.ThreadPoolExecutor(3).map(fetch,json.loads(LOCK.read_text())['files']): print(msg,flush=True)
