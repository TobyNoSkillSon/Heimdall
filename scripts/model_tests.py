#!/usr/bin/env python3
"""The model tests `scripts/check.sh` runs in its full (pre-install) mode: one at a time, under the GPU lock.

The default suite runs no model inference (User's rule): a test that loads a model is marked
`#[ignore = "model: <what it loads>"]`, so `cargo nextest run` and `--change`/`--tests` skip it, and the full check still
runs every case before an install. A marked test must pass with the checkout's models/ and no extra environment.
nextest cannot select by an ignore reason, so this reads the marks from the sources and prints a nextest filterset.

    scripts/model_tests.py            the workspace's marked tests except Splash's (empty when there are none)
    scripts/model_tests.py --splash   Splash's (it is tested on its own, check.sh)
"""
import json, re, subprocess, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MARK = re.compile(r'#\[ignore\s*=\s*"model:[^"]*"\s*\]')
FN = re.compile(r'\bfn\s+([A-Za-z_][A-Za-z0-9_]*)')


def packages():
    """Workspace package name -> its folder."""
    meta = json.loads(subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
                                     cwd=ROOT, check=True, capture_output=True, text=True).stdout)
    return {p["name"]: Path(p["manifest_path"]).parent for p in meta["packages"]}


def marked(folder):
    """Names of the marked test functions in a package's Rust files: the first `fn` after each mark."""
    names = set()
    for file in sorted(folder.rglob("*.rs")):
        if "target" in file.relative_to(folder).parts:
            continue
        text = file.read_text(errors="ignore")
        for mark in MARK.finditer(text):
            name = FN.search(text, mark.end())
            if name:
                names.add(name.group(1))
    return names


def filterset(by_package):
    """One nextest filterset: each marked test by its package and its last path segment."""
    return " | ".join(f"(package({package}) & test(/(^|::){name}$/))"
                      for package in sorted(by_package) for name in sorted(by_package[package]))


def main():
    splash = "--splash" in sys.argv[1:]
    found = {name: marked(folder) for name, folder in packages().items() if (name == "splash") == splash}
    print(filterset({name: tests for name, tests in found.items() if tests}))


if __name__ == "__main__":
    main()
