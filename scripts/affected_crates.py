#!/usr/bin/env python3
"""The workspace packages a change can affect, for `scripts/check.sh --change` (the day loop only).

Prints one package name per line, or ALL. A change outside crates/ (the root manifest, the lock file, nextest's
config, scripts, skills, docs, models, the toolchain) selects ALL: tests read those at run time. A change inside a
crate selects that crate, every workspace crate that depends on it, and every crate whose sources name a path in it
(`../speak/...`, read at run time). `scripts/check.sh` with no flag still runs every test before an install.

    scripts/affected_crates.py [BASE]      BASE defaults to main; uncommitted and untracked files count too
"""
import json, os, re, subprocess, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def git(*args):
    return subprocess.run(["git", "-C", str(ROOT), *args], check=True, capture_output=True, text=True).stdout


def changed_files(base):
    merge_base = git("merge-base", "HEAD", base).strip()
    files = set(git("diff", "--name-only", "--no-renames", merge_base).split())  # since base, working tree too; a rename counts both names
    files |= set(git("ls-files", "--others", "--exclude-standard").split())
    return sorted(files)


def packages():
    meta = json.loads(subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
                                     cwd=ROOT, check=True, capture_output=True, text=True).stdout)
    by_dir = {}
    for p in meta["packages"]:
        rel = Path(p["manifest_path"]).parent.relative_to(ROOT)
        by_dir[rel.as_posix()] = p
    names = {p["name"] for p in meta["packages"]}
    deps = {p["name"]: {d["name"] for d in p["dependencies"] if d["name"] in names} for p in meta["packages"]}
    return by_dir, deps


def path_reads(by_dir):
    """crate name -> names of other crates whose files its sources reference by a relative path, and
    crate name -> the lab/ paths its sources reference (`../../lab/phonon`)."""
    dir_to_name = {Path(d).name: p["name"] for d, p in by_dir.items()}
    pattern = re.compile(r"\.\./([A-Za-z0-9_-]+)/")
    lab = re.compile(r"\.\./\.\./(lab/[A-Za-z0-9_./-]*[A-Za-z0-9_-])")
    reads, lab_reads = {}, {}
    for d, p in by_dir.items():
        found, labs = set(), set()
        for f in (ROOT / d).rglob("*.rs"):
            if "target" in f.parts:
                continue
            text = f.read_text(errors="ignore")
            for name in pattern.findall(text):
                if name in dir_to_name and dir_to_name[name] != p["name"]:
                    found.add(dir_to_name[name])
            labs.update(lab.findall(text))
        reads[p["name"]], lab_reads[p["name"]] = found, labs
    return reads, lab_reads


def affected(files, by_dir, deps, reads, lab_reads):
    hit = set()
    for f in files:
        owner = next((d for d in by_dir if f == d or f.startswith(d + "/")), None)
        if owner is None:
            if f.startswith("crates/") and not f.endswith(".md"):
                return None  # a crate outside the workspace (crates/drama-engine): its own gates, so check everything
            if f in ("BRIEF.md", "COORD.md") or f.startswith("Assistant/"):
                continue  # a task's brief and notes, untracked; no test reads them
            if f.startswith("lab/"):
                # Experiments and their logs reach no test, except the lab fixtures a crate's sources name.
                hit |= {name for name, prefixes in lab_reads.items() if any(f.startswith(x) for x in prefixes)}
                continue
            return None
        hit.add(by_dir[owner]["name"])
    # Everything that depends on, or reads files of, a changed crate.
    users = {n: {m for m in deps if n in deps[m] or n in reads[m]} for n in deps}
    todo = list(hit)
    while todo:
        n = todo.pop()
        for m in users[n] - hit:
            hit.add(m)
            todo.append(m)
    return hit


def main():
    base = sys.argv[1] if len(sys.argv) > 1 else "main"
    files = changed_files(base)
    by_dir, deps = packages()
    result = affected(files, by_dir, deps, *path_reads(by_dir))
    if result is None:
        print("ALL")
    else:
        print("\n".join(sorted(result)))


if __name__ == "__main__":
    main()
