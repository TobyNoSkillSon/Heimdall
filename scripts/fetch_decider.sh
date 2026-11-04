#!/usr/bin/env bash
# Downloads Decider V6 in 8-bit from Hugging Face into models/decide/d1/d1-2B-heimdall-v6-8bit/ (about 2.1 GB)
# and checks every file against crates/decide/scripts/models.lock.json. Weights: LFM Open License v1.0.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="${HEIMDALL_DECIDER_REPO:-Toby-NoSkillSon/heimdall-d1-v6}"
DEST=models/decide/d1/d1-2B-heimdall-v6-8bit
mkdir -p "$DEST"
fetch() { # <remote path> <local name>
  local url="https://huggingface.co/$REPO/resolve/main/$1"
  [[ -s "$DEST/$2" ]] || curl -fL --retry 3 -C - -o "$DEST/$2.part" "$url" && { [[ -s "$DEST/$2" ]] || mv "$DEST/$2.part" "$DEST/$2"; }
}
for f in config.json calibration.json model.safetensors; do fetch "int8/$f" "$f"; done
for f in tokenizer.json tokenizer_config.json; do fetch "$f" "$f"; done
python3 - "$DEST" <<'PY'
import hashlib, json, sys, pathlib
dest = pathlib.Path(sys.argv[1])
rows = [r for r in json.load(open('crates/decide/scripts/models.lock.json'))['files'] if r['path'].startswith('d1/d1-2B-heimdall-v6-8bit/')]
bad = [r['path'] for r in rows if hashlib.sha256((dest / pathlib.Path(r['path']).name).read_bytes()).hexdigest() != r['sha256']]
sys.exit('checksum mismatch: ' + ', '.join(bad)) if bad else print(f'Decider V6 8-bit ready in {dest} ({len(rows)} files verified)')
PY
