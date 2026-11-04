#!/usr/bin/env python3
"""Build the native embedding helper and seal its relocatable, offline asset tree.

Agent-only. No app bundle, installation, model download or inference is performed.
The locked Swift packages provide C++/Swift and Metal from the same MLX revision.
"""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / 'crates/archive/embedding/native'
OUT = ROOT / 'models/embeddinggemma2/native'
# Agents on the user's Mac build with two jobs at low priority (HEIMDALL_BUILD_JOBS=2).
JOBS = os.environ.get('HEIMDALL_BUILD_JOBS', '4')


def run(args, **kwargs):
    subprocess.run([str(a) for a in args], check=True, timeout=kwargs.pop('timeout', 180), **kwargs)


def build():
    run(['/Library/Developer/CommandLineTools/usr/bin/swift', 'package', '--package-path', PACKAGE, 'resolve'], timeout=180)
    tokenizer = PACKAGE / '.build/checkouts/swift-transformers/Sources/Tokenizers/BPETokenizer.swift'
    data = tokenizer.read_text()
    if '// Heimdall: ranked neighbor heap' in data:
        start = data.index('    // Heimdall: ranked neighbor heap')
        end = data.index('    /// Tokenizes input text', start)
        revised = data[:start] + (PACKAGE / 'BPEHeap.swift.inc').read_text() + '\n' + data[end:]
        revised = revised.replace('let bpeTokens = bpe(token: text).split(separator: " ").map { String($0) }', 'let bpeTokens = bpe(token: text)')
        if revised != data:
            tokenizer.chmod(tokenizer.stat().st_mode | 0o200)
            tokenizer.write_text(revised)
    else:
        if hashlib.sha256(tokenizer.read_bytes()).hexdigest() != '1cdf782874825f60f191bf27529873fa1c5326e9efcdb8b69dfdf694fbe70734':
            raise RuntimeError('Tokenizer source differs from pinned patch target')
        start = data.index('    func bpe(token: String) -> String {')
        end = data.index('    /// Tokenizes input text', start)
        tokenizer.chmod(tokenizer.stat().st_mode | 0o200)
        revised = data[:start] + (PACKAGE / 'BPEHeap.swift.inc').read_text() + '\n' + data[end:]
        revised = revised.replace('let bpeTokens = bpe(token: text).split(separator: " ").map { String($0) }', 'let bpeTokens = bpe(token: text)')
        tokenizer.write_text(revised)
    normalizer = tokenizer.parent / 'Normalizer.swift'
    data = normalizer.read_text()
    marker = 'return text.replacingOccurrences(of: toReplace, with: replacement)'
    if '// Heimdall: normalize scalar spaces' not in data:
        assert data.count(marker) == 1
        data = data.replace(marker, '''// Heimdall: normalize scalar spaces even when followed by a combining mark.
            if toReplace == " " {
                var scalars = String.UnicodeScalarView()
                for scalar in text.unicodeScalars {
                    if scalar.value == 32 { scalars.append(contentsOf: replacement.unicodeScalars) }
                    else { scalars.append(scalar) }
                }
                return String(scalars)
            }
            return text.replacingOccurrences(of: toReplace, with: replacement)''')
        normalizer.chmod(normalizer.stat().st_mode | 0o200)
        normalizer.write_text(data)
    run(['/Library/Developer/CommandLineTools/usr/bin/swift', 'build', '--package-path', PACKAGE,
         '--build-system', 'native', '-c', 'release', '--jobs', JOBS], timeout=600)
    shaders = PACKAGE / '.build/checkouts/mlx-swift/Source/Cmlx/mlx-generated/metal'
    scratch = ROOT / 'target/embeddinggemma2/native-metal'
    scratch.mkdir(parents=True, exist_ok=True)
    # Like upstream's no-fast-math MLX build; this is shader compilation, not a GPU run.
    def compile(source):
        output = scratch / (source.stem + '.air')
        run(['xcrun', '-sdk', 'macosx', 'metal', '-std=metal3.1', '-fno-fast-math',
             '-I', shaders, '-c', source, '-o', output])
        return output
    with ThreadPoolExecutor(max_workers=int(JOBS)) as pool:
        objects = list(pool.map(compile, sorted(shaders.glob('*.metal'))))
    library = scratch / 'mlx.metallib'
    run(['xcrun', '-sdk', 'macosx', 'metallib', *objects, '-o', library])
    OUT.mkdir(parents=True, exist_ok=True)
    shutil.copy2(PACKAGE / '.build/release/heimdall-embedding-worker', OUT / 'heimdall-embedding-worker')
    shutil.copy2(library, OUT / 'mlx.metallib')
    # Tokenizers' local default configs are SwiftPM resources; carry the bundle intact.
    for bundle in (PACKAGE / '.build/release').glob('*.bundle'):
        for old in (OUT / bundle.name).rglob('*'):
            if old.is_file():
                old.chmod(old.stat().st_mode | 0o200)
        shutil.copytree(bundle, OUT / bundle.name, dirs_exist_ok=True)
    packages = json.loads((PACKAGE / 'Package.resolved').read_text())
    (OUT / 'source.json').write_text(json.dumps({'packages': packages,
        'precision': 'bfloat16', 'metal_flags': ['-std=metal3.1', '-fno-fast-math'],
        'tokenizer_patch_sha256': hashlib.sha256((PACKAGE / 'BPEHeap.swift.inc').read_bytes()).hexdigest(),
        'tokenizer_executed_source_sha256': hashlib.sha256(tokenizer.read_bytes() + normalizer.read_bytes()).hexdigest()}, indent=2) + '\n')
    rows = []
    for file in sorted(OUT.rglob('*')):
        if file.is_file():
            sha = hashlib.file_digest(file.open('rb'), 'sha256').hexdigest()
            rows.append({'path': file.relative_to(ROOT / 'models').as_posix(), 'sha256': sha,
                         'bytes': file.stat().st_size})
    tree = hashlib.sha256()
    for row in rows:
        tree.update((row['path'] + '\0' + row['sha256'] + '\n').encode())
    lock_path = ROOT / 'crates/archive/embedding/assets.lock.json'
    lock = json.loads(lock_path.read_text())
    lock.update(runtime_kind='native-mlx-swift', runtime_directory='embeddinggemma2/native',
                runtime_tree_sha256=tree.hexdigest(), runtime_files=len(rows),
                runtime_bytes=sum(row['bytes'] for row in rows),
                mlx_swift_lm_revision='c53d7d480147684f42dac9d959f648dc35f021d0')
    lock_path.write_text(json.dumps(lock, indent=2) + '\n')
    print(f'Native embedding assets: {len(rows)} files, {sum(row["bytes"] for row in rows)} bytes')


if __name__ == '__main__':
    build()
