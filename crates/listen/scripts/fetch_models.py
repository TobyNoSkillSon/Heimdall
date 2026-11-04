#!/usr/bin/env python3
"""Agent-operated pinned model fetch; verify all bytes, never consult moving HEAD."""
import concurrent.futures, hashlib, json, pathlib, urllib.request
ROOT = pathlib.Path(__file__).resolve().parents[3]
MANIFEST = pathlib.Path(__file__).with_name('models.lock.json')
def fetch(item):
    dst = ROOT / 'models' / item['path']
    if dst.exists() and hashlib.sha256(dst.read_bytes()).hexdigest() == item['sha256']:
        return
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_suffix(dst.suffix + '.download')
    h = hashlib.sha256()
    with urllib.request.urlopen(item['url'], timeout=60) as response, tmp.open('wb') as out:
        while chunk := response.read(1024 * 1024):
            h.update(chunk); out.write(chunk)
    if h.hexdigest() != item['sha256']:
        tmp.unlink(); raise RuntimeError('Checksum mismatch: ' + item['path'])
    tmp.replace(dst)
    print('Verified ' + item['path'], flush=True)
def main():
    manifest = json.loads(MANIFEST.read_text())
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        list(pool.map(fetch, manifest['files']))
if __name__ == '__main__': main()
