#!/usr/bin/env python3
"""check_public_bundle.py refuses a public bundle naming the build machine's home folder (VM finding P-03, 28 Oct) and
accepts one that names only remapped /build paths. No real bundle needed: a tiny fake one in a temp folder."""
import importlib.util, plistlib, sys, tempfile
from pathlib import Path

spec = importlib.util.spec_from_file_location('cpb', Path(__file__).with_name('check_public_bundle.py'))
cpb = importlib.util.module_from_spec(spec); spec.loader.exec_module(cpb)

with tempfile.TemporaryDirectory() as t:
    app = Path(t) / 'Heimdall Alpha.app'
    (app / 'Contents/Resources').mkdir(parents=True)
    (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({'HeimdallEdition': 'public'}))
    note = app / 'Contents/Resources/note.txt'
    note.write_text('panicked at /build/.cargo/registry/src/x.rs\n')
    problems, report = cpb.check(app, builder_home='/Users/builder')
    assert not problems and report['builder_home_files'] == 0, problems
    note.write_text('panicked at /Users/builder/Projects/x/src/main.rs\n')
    problems, report = cpb.check(app, builder_home='/Users/builder')
    assert report['builder_home_files'] == 1 and any('names the build machine' in p for p in problems), problems
    # Another user's home (a third-party build machine) is counted, not failed.
    note.write_text('/Users/runner/work/ort/x.c\n')
    problems, _ = cpb.check(app, builder_home='/Users/builder')
    assert not problems, problems
    # A half-remapped path keeps the checkout's folders under the home: refused when the source root is given.
    note.write_text('/build/Projects/x/crates/a.rs\n')
    problems, _ = cpb.check(app, builder_home='/Users/builder', source_root='/Users/builder/Projects/x')
    assert any('names the build machine' in p for p in problems), problems
    note.write_text('/build/heimdall/crates/a.rs\n')
    problems, _ = cpb.check(app, builder_home='/Users/builder', source_root='/Users/builder/Projects/x')
    assert not problems, problems
print('check_public_bundle: 5 cases ok')
