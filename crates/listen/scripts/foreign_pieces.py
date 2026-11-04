#!/usr/bin/env python3
"""Regenerate src/foreign-pieces.json: word-initial Parakeet v3 pieces that only other languages use.

Agent-run, offline. Inputs (local, never committed):
  VOCAB      models/parakeet-ultra/parakeet_v3_vocab.json
  WORDLISTS  a directory of OpenSubtitles 2018 frequency lists, `<lang>_50k.txt`
             (github.com/hermitdave/FrequencyWords, content/2018/<lang>/<lang>_50k.txt) for en, pl
             and Parakeet v3's other Latin-script languages: nl hr cs da de et fi fr hu it lv lt pt ro sk sl es sv
  TECHDOCS   a directory whose Markdown is English technical writing (the Vault's Projects/); words
             seen at least three times there are treated as English, so " var", " seg", " vec" stay.

A piece " xyz" is listed when no common English or Polish word (share >= 1e-6 of its list) and
no technical word starts with "xyz", while some other language uses it (prefix share >= 1e-4,
and >= 300x the English+Polish share). Letters outside the English/Polish alphabets are
handled in code (`vocabulary::foreign`) and are not listed here.

Usage: python3 -I foreign_pieces.py VOCAB WORDLISTS TECHDOCS OUT.json
"""
import collections, glob, json, re, sys

OTHER = "nl hr cs da de et fi fr hu it lv lt pt ro sk sl es sv".split()
WORD = re.compile(r"[^\W\d_]+")


def shares(path):
    counts = collections.Counter()
    for line in open(path, encoding="utf-8"):
        parts = line.split()
        if len(parts) == 2 and WORD.fullmatch(parts[0]):
            counts[parts[0].lower()] += int(parts[1])
    total = sum(counts.values())
    return {w: n / total for w, n in counts.items()}


def prefix_mass(words):
    mass = collections.Counter()
    for w, share in words.items():
        for i in range(1, min(len(w), 14) + 1):
            mass[w[:i]] += share
    return mass


def main(vocab_path, lists, docs, out):
    vocab = json.load(open(vocab_path))
    lang = {l: shares(f"{lists}/{l}_50k.txt") for l in ["en", "pl", *OTHER]}
    mass = {l: prefix_mass(w) for l, w in lang.items()}
    common = {w for l in ("en", "pl") for w, s in lang[l].items() if s >= 1e-6}
    tech = collections.Counter()
    for path in glob.glob(f"{docs}/**/*.md", recursive=True):
        if any(skip in path for skip in ("/node_modules/", "/target/", "/.claude/worktrees/")):
            continue
        text = open(path, encoding="utf-8", errors="ignore").read(200_000)
        tech.update(w.lower() for w in re.findall(r"[A-Za-z]{2,}", text))
    common |= {w for w, n in tech.items() if n >= 3}
    exempt = {w[:i] for w in common for i in range(1, min(len(w), 14) + 1)}
    listed = set()
    for piece in vocab.values():
        if piece.startswith("<") or not piece.startswith(" ") or re.search(r"[^a-zA-Z' ]", piece):
            continue
        p = piece[1:].lower()
        if not p or p in exempt:
            continue
        ours = mass["en"][p] + mass["pl"][p]
        theirs = max(mass[l][p] for l in OTHER)
        if theirs >= 1e-4 and theirs >= 300 * max(ours, 1e-9):
            listed.add(piece)
    json.dump(sorted(listed), open(out, "w"), ensure_ascii=False, indent=0)
    print(f"{len(listed)} pieces -> {out}")


if __name__ == "__main__":
    if len(sys.argv) != 5:
        sys.exit(__doc__)
    main(*sys.argv[1:])
