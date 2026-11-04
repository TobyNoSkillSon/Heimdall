#!/usr/bin/env bash
# Downloads every model the public app bundles into models/, each pinned to a revision and checked against its
# SHA-256 (about 4.5 GB). Safe to run again: verified files are skipped. Then build the native embedding helper.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 crates/listen/scripts/fetch_models.py            # Parakeet Ultra (Core ML), Silero VAD, Smart Turn v3.2
python3 crates/speak/scripts/fetch_phonon.py             # Phonon S voice
python3 crates/speakers/scripts/fetch_models.py          # Nemotron 3 Diarization, ReDimNet2 B6
scripts/fetch_decider.sh                                 # Decider V6, 8-bit
python3 - <<'PY'
# EmbeddingGemma 2 weights (crates/archive/embedding/assets.lock.json) and MossFormer2 (scripts/extra-models.lock.json).
import hashlib, json, pathlib, urllib.request
emb = json.load(open('crates/archive/embedding/assets.lock.json'))
jobs = [(r['path'], f"https://huggingface.co/{emb['model_id']}/resolve/{emb['revision']}/" + r['path'].split('/weights/', 1)[1], r['sha256'])
        for r in emb['files'] if '/weights/' in r['path']]
jobs.append(('night/model.int8.onnx', 'https://huggingface.co/ipsilondev/MossFormer2-SS-16K-ONNX/resolve/abd8b1268dfded6b0e972ddae4c2dd6a3faf4f8a/model.int8.onnx',
             json.load(open('scripts/extra-models.lock.json'))[0]['sha256']))
for path, url, sha in jobs:
    dst = pathlib.Path('models') / path
    if dst.exists() and hashlib.file_digest(dst.open('rb'), 'sha256').hexdigest() == sha:
        continue
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_suffix(dst.suffix + '.download'); h = hashlib.sha256()
    with urllib.request.urlopen(url, timeout=90) as r, tmp.open('wb') as out:
        while chunk := r.read(1 << 20):
            h.update(chunk); out.write(chunk)
    if h.hexdigest() != sha:
        tmp.unlink(); raise SystemExit('checksum mismatch: ' + path)
    tmp.replace(dst); print('Verified', path, flush=True)
PY
python3 scripts/build_embedding_native.py                # memory-search helper (Swift, MLX)
echo "Models ready in models/"
