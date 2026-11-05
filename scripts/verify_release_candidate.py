#!/usr/bin/env python3
"""Agent-only candidate QA: signed bundle, sealed assets and isolated offline replay."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = '51B366DDFD3BEA342E2AA8545D90146C9797F043'

def digest(path, algorithm='sha256'):
    with path.open('rb') as file:
        return hashlib.file_digest(file, algorithm).hexdigest()

def command(args):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, timeout=60)
    if result.returncode:
        raise RuntimeError(f'{args[0]} failed ({result.returncode}): {result.stdout.strip()}')
    return result.stdout.strip()

def requirement(bundle):
    text = command(['/usr/bin/codesign', '-d', '-r-', str(bundle)])
    return next(line for line in text.splitlines() if line.startswith('designated => '))

RUNTIME_SOURCES = ['package.json', 'package-lock.json', 'node-lock.json', 'heimdall.ts', 'tool-owner.cjs',
                   'loop-guard.mjs', 'loop-guard.json', 'provider-retry.mjs', 'thinking-policy.mjs', 'history-repair.mjs', 'service-tier.mjs', 'quota.mjs', 'compaction-fallback.mjs', 'arsenal.mjs']  # runtime::pin() order

def macho(path):
    with path.open('rb') as file:
        return file.read(4) in [b'\xcf\xfa\xed\xfe', b'\xca\xfe\xba\xbe']

# Floating Islands (night 1022): scripts/bundle_islands.sh puts the helper app and the islands CLI in the bundle.
ISLANDS_HELPER = 'Contents/Helpers/FloatingIslands.app'
ISLANDS_CLI = 'Contents/MacOS/islands'
ISLANDS_HELPER_ID = 'dev.heimdall.floatingislands'

def islands_code(bundle):
    """The Floating Islands code in the bundle, as (path, expected code identifier): the CLI is plain nested code
    like the speakers worker; the helper is its own bundle whose identifier matches its Info.plist."""
    helper, cli = bundle/ISLANDS_HELPER, bundle/ISLANDS_CLI
    executable = helper/'Contents/MacOS/FloatingIslands'
    assert cli.is_file() and macho(cli), 'islands CLI missing from Contents/MacOS'
    assert executable.is_file() and macho(executable), 'Floating Islands helper missing from Contents/Helpers'
    info = plistlib.loads((helper/'Contents/Info.plist').read_bytes())
    assert info.get('CFBundleIdentifier') == ISLANDS_HELPER_ID, 'Floating Islands helper identifier'
    assert info.get('CFBundleExecutable') == 'FloatingIslands' and info.get('LSUIElement') is True, 'Floating Islands helper Info.plist'
    return [(cli, 'dev.heimdall.app'), (helper, ISLANDS_HELPER_ID)]

def verify_agent_runtime(root):
    """Independent of the app's own check: sealed inventory, pin of the committed sources, exact pinned versions."""
    sources = ROOT/'crates/agent/runtime'
    receipt = json.loads((root/'receipt.json').read_text())
    pin = hashlib.sha256(b''.join((sources/name).read_bytes() for name in RUNTIME_SOURCES)).hexdigest()
    assert receipt['pin'] == pin, 'bundled agent runtime was built from other sources'
    files = {}
    for file in sorted(root.rglob('*')):
        assert not file.is_symlink(), f'agent runtime symlink: {file}'
        if file.is_file() and file != root/'receipt.json':
            files[file.relative_to(root).as_posix()] = digest(file)
    assert files == receipt['files'], 'agent runtime files missing, changed or added'
    for name in ['package.json', 'package-lock.json', 'heimdall.ts', 'tool-owner.cjs', 'loop-guard.mjs', 'loop-guard.json', 'provider-retry.mjs', 'thinking-policy.mjs', 'history-repair.mjs', 'service-tier.mjs', 'quota.mjs', 'compaction-fallback.mjs', 'arsenal.mjs']:
        assert (root/name).read_bytes() == (sources/name).read_bytes(), name
    lock = json.loads((sources/'package-lock.json').read_text())['packages']
    shipped = {}
    for manifest in root.glob('node_modules/**/package.json'):
        package = manifest.parent.relative_to(root).as_posix()
        if package in lock:
            shipped[package] = json.loads(manifest.read_text())['version']
            assert shipped[package] == lock[package]['version'], f'{package} is not the pinned version'
    tops = {p.relative_to(root).as_posix() for p in root.glob('node_modules/*') if not p.name.startswith('@')}
    tops |= {p.relative_to(root).as_posix() for p in root.glob('node_modules/@*/*')}
    assert tops <= set(shipped), f'packages outside the lock: {sorted(tops - set(shipped))}'
    node_lock = json.loads((sources/'node-lock.json').read_text())
    pi = json.loads((sources/'package.json').read_text())['dependencies']['@earendil-works/pi-coding-agent']
    env = {'PATH': '/usr/bin:/bin', 'HOME': os.environ['HOME'], 'PI_OFFLINE': '1', 'PI_TELEMETRY': '0'}
    node = root/'node/bin/node'
    assert subprocess.run([str(node), '--version'], capture_output=True, text=True, timeout=30, env=env).stdout.strip() == 'v'+node_lock['version']
    cli = root/'node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js'
    assert subprocess.run([str(node), str(cli), '--version'], capture_output=True, text=True, timeout=30, env=env).stdout.strip() == pi
    after = {f.relative_to(root).as_posix() for f in root.rglob('*') if f.is_file()} - {'receipt.json'}
    assert after == set(files), 'running Node/Pi wrote inside the bundle'
    sizes = {'node': node.stat().st_size, 'node_modules': sum(f.stat().st_size for f in (root/'node_modules').rglob('*') if f.is_file())}
    return {'pin': pin, 'node': node_lock['version'], 'pi': pi, 'packages': len(shipped), 'bytes': sizes}

def verify(bundle, evidence, lab=None):
    bundle = bundle.resolve(strict=True)
    evidence.mkdir(parents=True, exist_ok=False)
    models = bundle/'Contents/Resources/models'
    rows = json.loads((models/'manifest.json').read_text())
    for row in rows:
        file = models/row['path']
        assert file.is_file() and file.stat().st_size == row['bytes'], row['path']
        assert digest(file) == row['sealed_sha256'], row['path']
    paths = {row['path'] for row in rows}
    extra = json.loads((ROOT/'scripts/extra-models.lock.json').read_text())
    assert {row['path'] for row in extra} <= paths
    for row in extra:
        assert next(r for r in rows if r['path']==row['path'])['source_sha256']==row['sha256']
    verification = command(['/usr/bin/codesign', '--verify', '--deep', '--strict', str(bundle)])
    dr = requirement(bundle)
    installed = Path.home()/'Applications/Heimdall.app'
    assert dr == requirement(installed), 'installed designated requirement changed'
    plist = command(['/usr/bin/plutil','-extract','CFBundleIdentifier','raw',str(bundle/'Contents/Info.plist')])
    assert plist == 'dev.heimdall.app'
    # The user's install is the personal edition (docs/editions.md); a public build must never replace it.
    edition = command(['/usr/bin/plutil','-extract','HeimdallEdition','raw',str(bundle/'Contents/Info.plist')])
    assert edition == 'personal', f'candidate is the {edition} edition; User installs only personal'
    agent_runtime = bundle/'Contents/Resources/agent-runtime'
    runtime_code = sorted(f for f in agent_runtime.rglob('*') if f.is_file() and macho(f))
    assert agent_runtime/'node/bin/node' in runtime_code
    agent = verify_agent_runtime(agent_runtime)
    app_code = [bundle/'Contents/MacOS/heimdall-speakers-worker', *sorted(models.rglob('*.dylib')), *runtime_code]
    # Archive clips for tasks (docs/archive-clip.md), signed under its own identifier.
    nested = [(code, 'dev.heimdall.app') for code in app_code] + [(bundle/'Contents/MacOS/heimdall-clip', 'dev.heimdall.clip'), (bundle/'Contents/MacOS/heimdall-emergency', 'dev.heimdall.emergency')] + islands_code(bundle) + [(bundle, 'dev.heimdall.app')]
    identities = []
    for n, (code, identifier) in enumerate(nested):
        command(['/usr/bin/codesign','--verify','--strict',str(code)])
        detail = command(['/usr/bin/codesign','-dv',str(code)])
        assert f'Identifier={identifier}' in detail.splitlines(), f'code identifier: {code}'
        prefix = evidence/f'certificate-{n}-'
        command(['/usr/bin/codesign','-d','--extract-certificates='+str(prefix),str(code)])
        cert = Path(str(prefix)+'0')
        cert_sha1 = digest(cert,'sha1').upper()
        assert cert_sha1 == IDENTITY, f'nested code identity: {code}'
        identities.append({'path':str(code.relative_to(bundle)) if code != bundle else '.', 'certificate_sha1':cert_sha1})
    source = (bundle/'Contents/Resources/source-commit.txt').read_text().strip()
    # No UI/native capture/agent mode: the default bundled model-root selection is
    # deliberately used (no --models override); every child inherits that root.
    lab = (lab or Path(os.environ.get('CARGO_TARGET_DIR', ROOT/'target/release-prep-2'))/f'candidate-lab-{source[:12]}').resolve()
    assert not lab.exists(), 'choose a fresh lab directory; preserve previous evidence'
    executable = bundle/'Contents/MacOS/Heimdall'
    wav = ROOT/'crates/listen/tests/fixtures/en-2.wav'
    with (evidence/'replay.log').open('w') as log:
        child = subprocess.Popen([str(executable),'--night-replay-smoke','--replay',str(wav),
                                  '--data-dir',str(lab)], stdout=log, stderr=log, start_new_session=True)
        try:
            assert child.wait(timeout=220) == 0, 'candidate replay failed; inspect replay.log'
        except BaseException:
            if child.poll() is None:
                os.killpg(child.pid,signal.SIGKILL)
                child.wait(timeout=10)
            raise
    replay = json.loads((lab/'night-replay.json').read_text())
    assert replay['worker_starts']>=4
    assert replay['errors']==replay['gaps']==replay['sample_mismatches_from_input']==0
    # Canonical bundle tree digest: SHA256 of UTF-8 POSIX relative path + NUL +
    # file SHA256 hex + LF, sorted by relative path. Includes all sealed code,
    # model bytes, plist, signatures and licences. Receipts live outside the app.
    files = []
    tree = hashlib.sha256()
    for file in sorted(bundle.rglob('*')):
        assert not file.is_symlink(), f'unexpected bundle symlink: {file}'
        if file.is_file():
            relative = file.relative_to(bundle).as_posix()
            sha = digest(file)
            tree.update((relative+'\0'+sha+'\n').encode('utf-8'))
            files.append({'path':relative,'bytes':file.stat().st_size,'sha256':sha})
    receipt = {'candidate':str(bundle),'source_commit':source,'bundle_tree_sha256':tree.hexdigest(),
               'executable_sha256':digest(executable),'designated_requirement':dr,'same_as_installed':True,
               'codesign_verify_output':verification,'nested_code':identities,'runtime_assets':len(rows),
               'agent_runtime':agent,'lab':str(lab),'replay':replay,'files':files}
    (evidence/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
    (evidence/'bundle-sha256.txt').write_text(tree.hexdigest()+'  '+str(bundle)+' (canonical tree)\n')
    print(json.dumps({key:receipt[key] for key in ['candidate','source_commit','bundle_tree_sha256','executable_sha256','runtime_assets','agent_runtime','same_as_installed']}))

if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--bundle',type=Path,default=ROOT/'dist/Heimdall.app')
    parser.add_argument('--evidence',type=Path,required=True)
    parser.add_argument('--lab',type=Path,help='fresh isolated data directory for the replay')
    args=parser.parse_args()
    verify(args.bundle,args.evidence,args.lab)
