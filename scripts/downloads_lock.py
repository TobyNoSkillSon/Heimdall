#!/usr/bin/env python3
"""Writes crates/app/src/model_downloads.lock.json: what the public edition downloads on first run (docs/models.md,
"First-run downloads"), one group per model, each file with its official URL at a pinned revision, its size and its
SHA-256. Every row is checked against the locks it comes from (listen, decide, embedding, speakers); the only rows
that differ from the bundled ones are named in DECIDER_DIFFERS. Run it when a lock or the decider's revision changes.

    python3 scripts/downloads_lock.py           # writes the file
    python3 scripts/downloads_lock.py --check   # exits 1 when the file is out of date

Network: the Hugging Face API for the decider's tree (sizes and LFS SHA-256s) and the decider's small files (hashed
here, a few KB each). Nothing large is downloaded."""
import hashlib, json, sys, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'crates/app/src/model_downloads.lock.json'
DECIDER_REPO = 'Toby-NoSkillSon/heimdall-d1-v6'
DECIDER_REVISION = 'c015ecf6fed37b9477c27ab5790adb25ad9507df'
# The repo's folder per precision, and the app's (decide::precision::Pick::folder).
DECIDER = [('4bit', 'int4', 'd1/d1-2B-heimdall-v6-4bit'), ('8bit', 'int8', 'd1/d1-2B-heimdall-v6-8bit'), ('16bit', 'bf16', 'd1/d1-2B-heimdall-v6')]
# Published files that are not the bundled ones: each config.json drops `auto_map` (Python class names the Rust
# engine never reads), and bf16/model.safetensors is the text model alone with MLX names (3.46 GB; the bundled
# checkpoint carries the vision tower the engine skips). Same tensors, names, shapes and dtypes for the text model
# (headers compared 4 Nov 2026).
DECIDER_DIFFERS = {'config.json', 'bf16/model.safetensors'}
SPEAKERS = [
    ('speakers/Nemotron-3-Diarization.q8_0.gguf', 'https://huggingface.co/nvidia/Nemotron-3-Diarization/resolve/f667ed73aee57d40cc39428eb768b4fd87a0a29e/Nemotron-3-Diarization.q8_0.gguf'),
    ('speakers/ReDimNet2B6.onnx', 'https://huggingface.co/soniqo/ReDimNet2-B6-ONNX-FP32/resolve/e911e3f063899805f3d94ee5d1db53fff8e9f3e8/ReDimNet2B6.onnx'),
]


def get(url):
    with urllib.request.urlopen(url, timeout=60) as r:
        return r.read()


def lock_rows(path, key='files'):
    return {r['path']: r for r in json.loads((ROOT / path).read_text())[key]}


def build():
    groups = []
    listen = lock_rows('crates/listen/scripts/models.lock.json')
    groups.append(dict(id='parakeet', title='Parakeet', files=[
        dict(path=p, url=r['url'], sha256=r['sha256'], bytes=r['size']) for p, r in listen.items() if p.startswith('parakeet-ultra/')]))
    # The speaker models: the URLs crates/speakers/scripts/fetch_models.py pins, the SHA-256s bundle_models.py ships.
    fetch = (ROOT / 'crates/speakers/scripts/fetch_models.py').read_text()
    bundle = (ROOT / 'scripts/bundle_models.py').read_text()
    rows = []
    for path, url in SPEAKERS:
        line = next(l for l in fetch.splitlines() if url in l)
        size, sha = int(line.split(',')[2]), line.split("'")[-2]
        assert sha in bundle, f'{path}: {sha} is not the bundled one'
        rows.append(dict(path=path, url=url, sha256=sha, bytes=size))
    groups.append(dict(id='speakers', title='Speakers', files=rows))
    decide = lock_rows('crates/decide/scripts/models.lock.json')
    tree = {f['path']: f for f in json.loads(get(f'https://huggingface.co/api/models/{DECIDER_REPO}/tree/{DECIDER_REVISION}?recursive=true')) if f['type'] == 'file'}
    for key, remote, folder in DECIDER:
        rows = []
        for name in ['config.json', 'tokenizer.json', 'tokenizer_config.json', 'calibration.json', 'model.safetensors']:
            source = name if name.startswith('tokenizer') else f'{remote}/{name}'
            url = f'https://huggingface.co/{DECIDER_REPO}/resolve/{DECIDER_REVISION}/{source}'
            entry = tree[source]
            sha = (entry.get('lfs') or {}).get('oid') or hashlib.sha256(get(url)).hexdigest()
            local = decide[f'{folder}/{name}']
            same = (sha, entry['size']) == (local['sha256'], local['bytes'])
            differs = name in DECIDER_DIFFERS or source in DECIDER_DIFFERS
            assert same != differs, f'{source}: published {sha[:12]} vs bundled {local["sha256"][:12]}; update DECIDER_DIFFERS'
            rows.append(dict(path=f'decide/{folder}/{name}', url=url, sha256=sha, bytes=entry['size']))
        groups.append(dict(id=f'decider-{key}', title='Decider', files=rows))
    # The night pass's and speech split's separator (docs/night-pass.md): the revision docs/night-pass.md names, the
    # SHA-256 scripts/extra-models.lock.json ships.
    night = {r['path']: r for r in json.loads((ROOT / 'scripts/extra-models.lock.json').read_text())}['night/model.int8.onnx']
    url = 'https://huggingface.co/ipsilondev/MossFormer2-SS-16K-ONNX/resolve/abd8b1268dfded6b0e972ddae4c2dd6a3faf4f8a/model.int8.onnx'
    with urllib.request.urlopen(urllib.request.Request(url, method='HEAD'), timeout=60) as r:
        size = int(r.headers['Content-Length'])
    groups.append(dict(id='night', title='MossFormer2', files=[dict(path='night/model.int8.onnx', url=url, sha256=night['sha256'], bytes=size)]))
    embedding = json.loads((ROOT / 'crates/archive/embedding/assets.lock.json').read_text())
    base = f"https://huggingface.co/{embedding['model_id']}/resolve/{embedding['revision']}/"
    # The weights last: the memory-search worker starts once model.safetensors is in place (archive/src/embedding.rs).
    rows = sorted(embedding['files'], key=lambda r: r['path'].endswith('model.safetensors'))
    groups.append(dict(id='embedding', title='EmbeddingGemma 2', files=[
        dict(path=r['path'], url=base + r['path'].removeprefix('embeddinggemma2/weights/'), sha256=r['sha256'], bytes=r['bytes']) for r in rows]))
    return dict(note='Written by scripts/downloads_lock.py; what the public edition downloads (docs/models.md).', groups=groups)


if __name__ == '__main__':
    text = json.dumps(build(), indent=1) + '\n'
    if '--check' in sys.argv:
        sys.exit(0 if OUT.is_file() and OUT.read_text() == text else 'model_downloads.lock.json is out of date')
    OUT.write_text(text)
    lock = json.loads(text)
    for g in lock['groups']:
        print(f"{g['id']}: {len(g['files'])} files, {sum(f['bytes'] for f in g['files'])} bytes")
