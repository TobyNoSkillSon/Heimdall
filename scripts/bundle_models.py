#!/usr/bin/env python3
"""Copy only pinned runtime assets; never copy evaluation data or recordings.

Editions (docs/editions.md): `personal` (User's install) copies everything as before; `public` leaves out Drama 3 and
its Python runtime, Supertonic, and the Smart Turn model tuned on User's speech (the stock one ships in both)."""
import argparse, hashlib, json, shutil
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]

def digest(p):
    with p.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def stage(destination, edition='personal'):
    public = edition == 'public'
    destination.mkdir(parents=True, exist_ok=True)
    entries = []
    listen = json.loads((ROOT/'crates/listen/scripts/models.lock.json').read_text())
    for row in listen['files']:
        entries.append((row['path'], row['sha256']))
    speak = json.loads((ROOT/'crates/speak/scripts/models.lock.json').read_text())
    for row in speak['assets'] if not public else []:
        entries.append(('supertonic-3/'+row['file'], row['sha256']))
    # Phonon S (night 1022 menu): model, tokenizer, the 37 English voice presets and their list (fetch_phonon.py).
    phonon = json.loads((ROOT/'crates/speak/scripts/phonon.lock.json').read_text())
    for row in phonon['assets']:
        entries.append(('phonon-31m-en/'+row['file'], row['sha256']))
    # Decider V6 (the default, 4 Nov) and V5 (the rollback, `decider_model: "v5"`) ship in all three precisions each
    # (16-bit, 8-bit, 4-bit folders: decide::precision::Pick), so the menu's and Setup's precision pick and the version
    # setting hot-swap without a download (30 Oct, 4 Nov). Every `bundle: true` d1 row of the lock ships.
    decide = json.loads((ROOT/'crates/decide/scripts/models.lock.json').read_text())
    for row in decide['files']:
        if row['path'].startswith('d1/') and row.get('bundle', True):
            # Public edition: Decider V6 in 8-bit only (huggingface.co/Toby-NoSkillSon/heimdall-d1-v6, int8/;
            # scripts/fetch_decider.sh). The app resolves its precision pick to the nearest folder present.
            if public and not row['path'].startswith('d1/d1-2B-heimdall-v6-8bit/'):
                continue
            entries.append(('decide/'+row['path'], row['sha256']))
    # The decider's runtime: Decider V5's MLX engine and its metallib (scripts/build_decider_v5.sh). The llama.cpp
    # runtime for the 600M GGUFs ships only while its lock says bundle.
    runtime=json.loads((ROOT/'crates/decide/scripts/runtime.lock.json').read_text())
    if runtime.get('bundle', True):
        entries.append(('decide/'+runtime['path'], runtime['sha256']))
    for row in json.loads((ROOT/'crates/decide/scripts/runtime-v5.lock.json').read_text())['files']:
        if row.get('bundle', True):
            entries.append(('decide/'+row['path'], row['sha256']))
    entries.extend([
        ('speakers/Nemotron-3-Diarization.q8_0.gguf', '08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1'),
        ('speakers/ReDimNet2B6.onnx', 'd960e9f713eaba712f44498e82221b6fc12fcd7eaacd367c9eac45d3a6ce59ec'),
    ])
    for row in json.loads((ROOT/'scripts/extra-models.lock.json').read_text()):
        entries.append((row['path'], row['sha256']))
    embedding=json.loads((ROOT/'crates/archive/embedding/assets.lock.json').read_text())
    runtime_tree=hashlib.sha256();runtime_rows=[]
    for file in sorted((ROOT/'models'/embedding.get('runtime_directory','embeddinggemma2/runtime')).rglob('*')):
        if file.is_file() and '__pycache__' not in file.parts and '.cache' not in file.parts:
            relative=file.relative_to(ROOT/'models').as_posix();sha=digest(file)
            runtime_tree.update((relative+'\0'+sha+'\n').encode());runtime_rows.append((relative,sha))
    if runtime_tree.hexdigest()!=embedding['runtime_tree_sha256'] or len(runtime_rows)!=embedding['runtime_files']:
        raise RuntimeError('Missing or changed embedding runtime')
    entries.extend(runtime_rows)
    for row in embedding['files']:
        entries.append((row['path'],row['sha256']))
    # Engine/decider activate these local exports only as complete model-policy pairs.
    # Preserve the exact development runtime selection in the bundled resource root.
    # Training checkpoints, corpus and receipts are deliberately excluded.
    for pair in [('smart-turn/owner-turn.onnx','smart-turn/owner-policy.json')] if not public else []:
        present=[(ROOT/'models'/relative).is_file() for relative in pair]
        if any(present) and not all(present):
            raise RuntimeError('Incomplete active runtime model/policy pair: '+str(pair))
        if all(present):
            entries.extend((relative,digest(ROOT/'models'/relative)) for relative in pair)
    # Smart Turn defaults to FP16 Metal; its derived assets are hash-locked to the ONNX
    # at load time, so ship them with the tuned pair or the app would run on CPU.
    if not public and (ROOT/'models/smart-turn/owner-turn.onnx').is_file():
        for relative in ['smart-turn/owner-turn.metal.json','smart-turn/owner-turn.metal.bin']:
            if not (ROOT/'models'/relative).is_file():
                raise RuntimeError('Missing Smart Turn Metal asset: '+relative)
            entries.append((relative,digest(ROOT/'models'/relative)))
    # The Python closure is sealed by the same manifest/signing path as other helpers.
    # Development-only interpreter and 23.8 GB weights never enter the app.
    manifest=[]
    if not public:
        import os
        import drama3_prepare
        staged=drama3_prepare.ensure_prepared(ROOT/'lab/drama3',ROOT/'target/drama3-prepared',
            force=os.environ.get('HEIMDALL_DRAMA3_PREPARE')=='rebuild')
        shutil.copytree(staged,destination/'drama3',ignore=shutil.ignore_patterns('preparation.json'))
        # The sidecar runs under Heimdall's name, so Activity Monitor lists Drama 3's memory as Heimdall's and not as
        # "python3" (crates/speak/src/drama3.rs `SIDECAR_PROCESS`): the same interpreter, linked beside it.
        python=destination/'drama3/runtime/bin/python3'
        os.link(python,python.with_name('Heimdall Drama 3'))
        # The Rust Drama engine (crates/drama-engine), the default when bundled (HEIMDALL_DRAMA3_ENGINE=python keeps
        # the Python worker, which also takes over if it fails to start); it links the
        # runtime's own libmlx (rpath @executable_path/../runtime/...). The personal build refuses to ship without it
        # (night 4 Nov: a cleanup had deleted the engine build, the bundle skipped it silently and Jarvis fell back to
        # Python); HEIMDALL_DRAMA3_ENGINE=python bundles the Python worker alone on purpose.
        rust=ROOT/'crates/drama-engine/target/release/heimdall-drama3-engine'
        if not rust.is_file() and os.environ.get('HEIMDALL_DRAMA3_ENGINE')!='python':
            raise SystemExit(f'{rust} is missing: build it (cd crates/drama-engine && nice -n 19 cargo build --release), '
                             'or set HEIMDALL_DRAMA3_ENGINE=python to ship the Python worker alone')
        if rust.is_file():
            # The default engine: never ship one older than its source (review 1 Nov).
            engine=ROOT/'crates/drama-engine'
            sources=[engine/'Cargo.toml',engine/'Cargo.lock',*(engine/'src').rglob('*'),
                     *(f for f in (engine/'vendor').rglob('*') if 'target' not in f.relative_to(engine).parts)]
            newest=max(f.stat().st_mtime for f in sources if f.is_file())
            if rust.stat().st_mtime<newest:
                raise SystemExit(f'{rust} is older than crates/drama-engine: rebuild it (cargo build --release there)')
            (destination/'drama3/rust').mkdir()
            shutil.copy2(rust,destination/'drama3/rust/Heimdall Drama 3')
        manifest=[dict(path=p.relative_to(destination).as_posix(),bytes=p.stat().st_size,source_sha256=digest(p))
                  for p in sorted((destination/'drama3').rglob('*')) if p.is_file()]
    for relative, expected in entries:
        source=ROOT/'models'/relative
        if not source.is_file() or digest(source)!=expected:
            raise RuntimeError('Missing or changed runtime asset: '+relative)
        target=destination/relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source,target)
        if digest(target)!=expected:
            raise RuntimeError('Copy verification failed: '+relative)
        manifest.append(dict(path=relative,bytes=target.stat().st_size,source_sha256=expected))
    (destination/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print(f'Bundled {len(manifest)} {edition} runtime assets; {sum(r["bytes"] for r in manifest)} bytes; no corpus/QA files')

if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('destination',type=Path)
    p.add_argument('--edition',choices=['public','personal'],default='personal');a=p.parse_args();stage(a.destination,a.edition)
