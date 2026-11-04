#!/usr/bin/env python3
"""The public edition's content rule (docs/editions.md): no Python, nothing of User's, every runner native and inside
the app. Exits non-zero on a violation; prints one JSON report (also to --report).

- No Python: no .py/.pyc files, no python executables or site-packages, no script whose #! names python.
- Native and inside: every Mach-O file loads only macOS (/System/Library, /usr/lib) or the bundle (@rpath,
  @loader_path, @executable_path), and a file that loads through @rpath searches only macOS and the bundle. A
  toolchain search path in a file that never loads through @rpath (SwiftPM adds one), or one searched only after
  macOS and the app, is listed, not failed.
- Nothing of User's: no Drama 3, Supertonic or tuned Smart Turn assets; Info.plist says HeimdallEdition public.

- No build machine: no file names the builder's home folder (Rust panic locations and C `__FILE__` strings did before
  scripts/bundle.sh remapped them; VM finding P-03, 28 Oct). Other `/Users/` strings in code (third-party build
  machines, macOS folder names) are counted, not failed."""
import argparse, json, mmap, os, plistlib, re, subprocess, sys
from pathlib import Path

MACHO = {b'\xcf\xfa\xed\xfe', b'\xfe\xed\xfa\xcf', b'\xca\xfe\xba\xbe', b'\xbe\xba\xfe\xca', b'\xca\xfe\xba\xbf', b'\xbf\xba\xfe\xca'}
SYSTEM = ('/System/Library/', '/usr/lib/')
INSIDE = ('@rpath/', '@loader_path/', '@executable_path/')
LOADS = re.compile(r'cmd (LC_LOAD_DYLIB|LC_LOAD_WEAK_DYLIB|LC_REEXPORT_DYLIB|LC_LAZY_LOAD_DYLIB|LC_LOAD_UPWARD_DYLIB)\n\s+cmdsize \d+\n\s+name (.+?) \(offset')
RPATHS = re.compile(r'cmd LC_RPATH\n\s+cmdsize \d+\n\s+path (.+?) \(offset')
PERSONAL_MODELS = ('drama3/', 'supertonic-3/', 'smart-turn/owner-')

def macho(path):
    with path.open('rb') as f:
        return f.read(4) in MACHO

def otool(*args):
    return subprocess.run(['/usr/bin/otool', *args], capture_output=True, text=True, check=True).stdout

def check(bundle, builder_home=None, source_root=None):
    home = (builder_home or os.path.expanduser('~')).rstrip('/')
    builder = home.encode()
    # The checkout's folders under the home, as a half-remapped path would keep them (/build/<folders>/<checkout>/...).
    tail = os.path.relpath(source_root, home).encode() if source_root and source_root.startswith(home + '/') else None
    problems, files = [], [p for p in sorted(bundle.rglob('*')) if p.is_file() and not p.is_symlink()]
    rel = lambda p: p.relative_to(bundle).as_posix()
    info = plistlib.loads((bundle/'Contents/Info.plist').read_bytes())
    if info.get('HeimdallEdition') != 'public':
        problems.append(f"Info.plist HeimdallEdition is {info.get('HeimdallEdition')!r}")
    python = [rel(p) for p in files if p.suffix in ('.py', '.pyc', '.pyo', '.pth') or re.match(r'python[0-9.]*$', p.name)
              or 'site-packages' in p.parts or any(re.match(r'python3\.[0-9]+$', part) for part in p.parts)]
    shebang = []
    for p in files:
        if p.stat().st_size and not macho(p):
            with p.open('rb') as f:
                head = f.read(128)
            if head.startswith(b'#!') and b'python' in head.split(b'\n', 1)[0]:
                shebang.append(rel(p))
    problems += [f'Python file: {p}' for p in python] + [f'Python script: {p}' for p in shebang]
    personal = [rel(p) for p in files if any(rel(p).startswith('Contents/Resources/models/'+m) for m in PERSONAL_MODELS)]
    problems += [f'personal model: {p}' for p in personal]
    code, links, unused_rpaths, fallback_rpaths, path_strings = [], {}, [], [], 0
    for p in files:
        if not macho(p):
            continue
        code.append(rel(p))
        commands = otool('-l', str(p))
        loads = {lib for _, lib in LOADS.findall(commands)}
        for lib in loads:
            links[lib] = links.get(lib, 0) + 1
            if not lib.startswith(SYSTEM + INSIDE):
                problems.append(f'{rel(p)} loads outside macOS and the app: {lib}')
        rpaths = list(dict.fromkeys(RPATHS.findall(commands)))  # in search order
        outside = sorted(r for r in rpaths if not (r.startswith(('@loader_path', '@executable_path')) or r.startswith(SYSTEM)))
        through = sorted(lib for lib in loads if lib.startswith('@rpath/'))
        if outside and not through:
            unused_rpaths += [f'{rel(p)}: {r}' for r in outside]
        elif outside and len(outside) == len(rpaths):
            problems.append(f'{rel(p)} loads {through} only from outside the app: {outside}')
        elif outside:
            # Found in macOS or the app first; the outside path is a fallback on systems that lack the library.
            fallback_rpaths.append(f'{rel(p)} loads {", ".join(through)} from macOS or the app, else from {", ".join(outside)}')
        path_strings += p.read_bytes().count(b'/Users/')
    def mentions(p):  # mapped, not read into memory: model weights are gigabytes
        if not p.stat().st_size:
            return 0
        with p.open('rb') as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
            n = 0
            for needle in [builder + b'/'] + ([b'/build/' + tail] if tail else []):
                i = m.find(needle)
                while i >= 0:
                    n, i = n + 1, m.find(needle, i + 1)
            return n
    builder_files = [(rel(p), n) for p in files if (n := mentions(p))]
    problems += [f'names the build machine ({n}x): {f}' for f, n in builder_files[:20]]
    frameworks = sorted({re.sub(r'.*/([^/]+)\.framework/.*', r'\1', l) for l in links if '.framework/' in l and l.startswith('/System/')})
    return problems, {
        'bundle': str(bundle), 'edition': info.get('HeimdallEdition'), 'files': len(files),
        'bytes': sum(p.stat().st_size for p in files), 'macho_files': len(code), 'python_files': len(python) + len(shebang),
        'personal_models': len(personal), 'external_links': [l for l in links if not l.startswith(SYSTEM + INSIDE)],
        'system_frameworks': frameworks, 'system_libraries': sorted(l for l in links if l.startswith('/usr/lib/')),
        'unused_rpaths': unused_rpaths, 'fallback_rpaths': fallback_rpaths, 'build_path_strings_in_code': path_strings,
        'builder_home_files': len(builder_files), 'problems': problems,
    }

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('bundle', type=Path)
    parser.add_argument('--report', type=Path)
    parser.add_argument('--source-root', help="the checkout the bundle was built from; its folders must not survive remapping")
    args = parser.parse_args()
    problems, report = check(args.bundle.resolve(strict=True), source_root=args.source_root)
    text = json.dumps(report, indent=2)
    if args.report:
        args.report.write_text(text + '\n')
    print(text)
    if problems:
        print('PUBLIC BUNDLE REFUSED: ' + '; '.join(problems[:20]), file=sys.stderr)
        sys.exit(1)
