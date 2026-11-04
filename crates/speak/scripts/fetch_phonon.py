#!/usr/bin/env python3
"""Fill models/phonon-31m-en from pinned Phonon S files and verify them against phonon.lock.json.

Agent-run development machinery, never a runtime dependency (the app reads only its bundled copy). Sources, in order:
a local Hugging Face hub cache (--hub, default lab/phonon/hf/hub), else the pinned URL. `--lock` rewrites the lock
from the files in place (only when the pins change).
"""
import argparse, hashlib, json, shutil, urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
MODEL = ('gradium/phonon-31m-en', '541f5752bc3c79583068bb205bfda5eb39aaa08a')
VOICES = ('gradium/phonon-voices', 'da6c9ec0d47be8df5c87c730a3aa97187f3229c3')
MODEL_FILES = ['config.json', 'tokenizer.json', 'model.q8.gguf', 'default-voice.safetensors']
LOCAL = ['voices.json', 'NOTICE.md']  # Heimdall's own, from crates/speak/phonon


def digest(p):
    with p.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def sources():
    presets = json.loads((HERE.parent / 'phonon/voices.json').read_text())
    rows = [(f, MODEL, f) for f in MODEL_FILES]
    rows += [(f'voices/{p["id"]}.safetensors', VOICES, f'voices/{p["id"]}.safetensors') for p in presets]
    return rows


def fetch(target, repo, revision, remote, hub):
    cached = hub / ('models--' + repo.replace('/', '--')) / 'snapshots' / revision / remote
    target.parent.mkdir(parents=True, exist_ok=True)
    if cached.is_file():
        shutil.copyfile(cached.resolve(), target)
        return
    url = f'https://huggingface.co/{repo}/resolve/{revision}/{remote}'
    tmp = target.with_suffix(target.suffix + '.partial')
    with urllib.request.urlopen(url, timeout=60) as response, tmp.open('wb') as out:
        while chunk := response.read(1 << 20):
            out.write(chunk)
    tmp.replace(target)


def main():
    a = argparse.ArgumentParser()
    a.add_argument('--hub', type=Path, default=ROOT / 'lab/phonon/hf/hub')
    a.add_argument('--dest', type=Path, default=ROOT / 'models/phonon-31m-en')
    a.add_argument('--lock', action='store_true')
    args = a.parse_args()
    lock_path = HERE / 'phonon.lock.json'
    rows = []
    for relative, (repo, revision), remote in sources():
        target = args.dest / relative
        if not target.is_file():
            fetch(target, repo, revision, remote, args.hub)
        rows.append({'file': relative, 'bytes': target.stat().st_size, 'sha256': digest(target),
                     'url': f'https://huggingface.co/{repo}/resolve/{revision}/{remote}'})
    for name in LOCAL:
        target = args.dest / name
        shutil.copyfile(HERE.parent / 'phonon' / name, target)
        rows.append({'file': name, 'bytes': target.stat().st_size, 'sha256': digest(target), 'url': None})
    if args.lock:
        lock = {'model': '@'.join(MODEL), 'voices': '@'.join(VOICES), 'assets': rows,
                'total_bytes': sum(r['bytes'] for r in rows)}
        lock_path.write_text(json.dumps(lock, indent=2) + '\n')
    expected = {r['file']: r['sha256'] for r in json.loads(lock_path.read_text())['assets']}
    bad = [r['file'] for r in rows if expected.get(r['file']) != r['sha256']] + sorted(set(expected) - {r['file'] for r in rows})
    if bad:
        raise SystemExit('Phonon assets differ from phonon.lock.json: ' + ', '.join(bad))
    print(f'Phonon S: {len(rows)} files, {sum(r["bytes"] for r in rows)} bytes, all match phonon.lock.json')


if __name__ == '__main__':
    main()
