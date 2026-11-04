#!/bin/zsh
# Builds the Decider V5 engine (crates/decide/engine-v5, Rust on MLX) and places its runtime where the app loads it:
#   models/decide/runtime-v5/libd1v5_engine.dylib   the C ABI Heimdall's d1 loader opens (crates/decide/src/d1.rs)
#   models/decide/runtime-v5/mlx.metallib           MLX's Metal kernels; MLX looks for it beside the dylib first
# Low priority (nice 19, two jobs) and offline: MLX v0.32.2 and its C++ dependencies come from
# crates/decide/engine-v5/target/deps-src (build/fetch-deps.sh fills it, from a local MLX build when one exists), and the
# Rust crates from the local cargo registry (~/.cargo/registry). Nothing vendored goes into git.
#
#   scripts/build_decider_v5.sh          build and place the runtime; prints bytes and sha256 of both files
#   scripts/build_decider_v5.sh --lock   the same, then rewrites crates/decide/scripts/runtime-v5.lock.json to match
#
# The dylib embeds its build folder's paths, so a build in another checkout has another sha256. bundle_models.py refuses
# files that differ from the lock: after rebuilding elsewhere, run with --lock and commit the lock.
set -eu
project=${0:A:h:h}
engine=$project/crates/decide/engine-v5
runtime=${HEIMDALL_D1V5_RUNTIME_DIR:-$project/models/decide/runtime-v5}
cd $engine
if [[ ! -d target/deps-src/mlx-src || ! -d target/deps-src/metal_cpp-src ]]; then
  build/fetch-deps.sh
fi
CARGO_BUILD_JOBS=2 CMAKE_BUILD_PARALLEL_LEVEL=2 nice -n 19 ${CARGO:-$HOME/.cargo/bin/cargo} build --release --offline
dylib=target/release/libd1v5_engine.dylib
metallib=target/mlx-metal/mlx.metallib
[[ -f $dylib && -f $metallib ]] || { echo "engine build produced no dylib or metallib" >&2; exit 1; }
mkdir -p $runtime
for f in $dylib $metallib; do
  cp $f $runtime/${f:t}.tmp
  mv -f $runtime/${f:t}.tmp $runtime/${f:t}
done
python3 - "$runtime" "$project/crates/decide/scripts/runtime-v5.lock.json" "${1:-}" <<'PY'
import hashlib, json, subprocess, sys
from pathlib import Path
runtime, lock, mode = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
rows = []
for name in ['libd1v5_engine.dylib', 'mlx.metallib']:
    p = runtime/name
    rows.append(dict(path='runtime-v5/'+name, bytes=p.stat().st_size, sha256=hashlib.file_digest(p.open('rb'), 'sha256').hexdigest()))
    print(f"{rows[-1]['path']}  {rows[-1]['bytes']} bytes  sha256 {rows[-1]['sha256']}")
data = json.loads(lock.read_text())
if mode == '--lock':
    commit = subprocess.run(['git', '-C', str(lock.parent), 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip()
    for row in rows:
        entry = next(f for f in data['files'] if f['path'] == row['path'])
        entry.update(bytes=row['bytes'], sha256=row['sha256'])
    data['built_at_commit'] = commit
    lock.write_text(json.dumps(data, indent=2) + '\n')
    print('lock updated:', lock)
else:
    locked = {f['path']: f['sha256'] for f in data['files']}
    stale = [r['path'] for r in rows if locked.get(r['path']) != r['sha256']]
    print('matches the lock' if not stale else 'differs from the lock (run with --lock): ' + ', '.join(stale))
PY
