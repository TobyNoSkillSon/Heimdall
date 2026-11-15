#!/usr/bin/env python3
"""The public/private split of Heimdall's prompt and skills (docs/skills.md, "Two layers"), enforced.

Public text is true for any user and ships in the app and the public repository. Private text is one user's own and
lives only in their data folder. This script is the one place that says which files are which, and it fails when
private text could reach a public tree.

  check_public_split.py                         check this repository's public layer
  check_public_split.py --list                  print the prompt and skill files a public export may take
  check_public_split.py --tree DIR              check a whole exported tree or an app bundle
  check_public_split.py --private DIR [...]     also search for one user's private text (their private folder)

It fails on any of:

1. A skill with no class, or an unknown one (`metadata:` / `class: public|private` in its frontmatter).
2. A private-class file in a public place: a `class: private` skill under `skills/` or in the tree.
   The public layer includes the app's own model prompts (`crates/app/prompt/`, both editions' copies).
3. A private path: a folder only one user's material lives in (`PRIVATE_PATHS`).
4. A private string in the public layer (the general prompts and the public skills): the built-in patterns, which
   name no one (`LAYER_PATTERNS`: a home path, a vault, a dated quote, and one user's model routing stated as a rule,
   that is a provider prescribed or forbidden, or a model named as the one to use), and the strings of
   `--private DIR/private-strings.txt`.
5. A line of a private file (`--private DIR`, every `.md` under it) found in a public file: with `--tree`, in every
   text file of the tree.

The built-in patterns carry no private string themselves, so this file ships. One user's names, projects and
accounts are in their own `private-strings.txt`, which never does.
"""
import argparse, json, re, sys
from pathlib import Path

# What a public export may take of the prompt and the skills, relative to the repository root. Everything else under
# `crates/agent/prompt/` and `skills/` stays behind.
PUBLIC_PROMPTS = ("crates/agent/prompt/heimdall.md", "crates/agent/prompt/worker.md", "crates/agent/prompt/relay.md",
                  "crates/agent/prompt/compaction.md", "crates/agent/prompt/relay.generated.md")
# The app's own model prompts (the island agent, the plan refresher, Goals, the suggester): text the app sends to a model.
# Both editions' copies ship as source, so they are read as part of the public layer.
APP_PROMPT_DIRS = ("crates/app/prompt",)
# `crates/agent/prompt/taste-review.md` is the guide the taste ledger's review command prints. It names no one, but it
# belongs to that feature: an export that leaves the taste ledger out leaves it out too, so it is not listed here.
PUBLIC_SKILL_SETS = ("skills/public", "skills/shared")

# Folders that hold one user's material. None may exist in a public tree or bundle. A path matches when it contains
# one of these as whole path segments.
PRIVATE_PATHS = (
    "prompt/personal",      # one user's prompt text never sits beside the general prompts
    "agent/prompt/public",  # the old forked copies: one general prompt replaced them
    "skills/heimdall",      # the old personal skill sets
    "skills/workers",
    "skills/private",
    "agent/private",        # a user's private layer, if a data folder were ever swept into a tree
    "lab/prompt-split",     # the staged private layer of the app's developer
    "docs/personal",
    "agent/tests/private_layer.rs",  # the pins of the developer's own private layer
)

# Patterns no public prompt or skill may match. They name no one: a home path, a personal vault, a dated quote of a
# rule ("(Sam, 14 Nov)", "14 Nov 09:26"), which only one user's standing rules carry.
MONTHS = "Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec"
LAYER_PATTERNS = (
    ("home path", re.compile(r"/Users/(?!x/|someone/|you/|<)[A-Za-z0-9._-]+/")),
    ("personal vault", re.compile(r"(?<![A-Za-z])Vault(?:/|\b)")),
    ("dated quote", re.compile(rf"\(\s*[A-Z][a-z]+,\s+\d{{1,2}} (?:{MONTHS})\b")),
    ("dated quote", re.compile(rf"\b\d{{1,2}} (?:{MONTHS}) \d{{1,2}}:\d{{2}}\b")),
    ("dated quote", re.compile(rf"\(\d{{1,2}} (?:{MONTHS})(?: 20\d\d)?\)")),
    # Routing is the user's: general text says how a model is picked, never which provider or model ("general skills
    # state the mechanism, his private layer states his choices", 14 Nov). A provider prescribed or forbidden:
    ("provider rule", re.compile(
        r"(?i)\b(?:never|only)\b[^.\n]{0,60}\b(?:codex|anthropic|chatgpt|openai|grok|opencode)\b(?! code)(?! routes)"
        r"|\b(?:anthropic|claude|chatgpt|codex|openai|grok)\s+(?:subscription\s+|account\s+)?only\b"
        r"|\bon the (?:anthropic|openai|claude|chatgpt|codex) (?:subscription|account)\b")),
)
# A model named in general text reads as the one to use. Harness names (Claude Code, Codex) are not models.
MODEL_NAMED = re.compile(r"(?i)\bclaude[- ](?:opus|haiku|sonnet|fable)\b|\b(?:opus|haiku|sonnet|fable) \d|\bgpt-\d|\bclaude-[a-z]+-\d")
# The one general file that may name models: the speed skill's picks are the developer's dated opinion, said as such
# to a user who asks which model is fastest, and bind nothing ("Present these as the developer's view with its date").
MODEL_NAMES_ALLOWED_IN = ("skills/public/speed-and-models/SKILL.md",)
# Made-up dates that show a format: the Now line the app's clock writes, the compaction text's examples, a memory
# note's "Updated" line. A new example is added here by hand, after a look.
ALLOWED_LAYER = ("[Now: Tuesday 20 Oct 2026, 19:22]", "20 Oct 19:04", "20 Oct 18:41", "8 Nov 09:12")

TEXT_SUFFIXES = {".md", ".rs", ".py", ".sh", ".json", ".toml", ".txt", ".mjs", ".ts", ".js", ".html", ".css", ".yml",
                 ".yaml", ".plist", ".swift", ".c", ".h", ".rb", ".lock", ""}
FINGERPRINT_MIN = 48


def frontmatter(text):
    if not text.startswith("---\n"):
        return None
    end = text.find("\n---", 4)
    return None if end < 0 else text[4:end]


def skill_class(text):
    """The class a SKILL.md declares: `class:` indented under `metadata:`; None when absent or unknown."""
    front = frontmatter(text)
    if front is None:
        return None
    in_metadata = False
    for line in front.splitlines():
        if not line.startswith((" ", "\t")):
            in_metadata = line.rstrip() == "metadata:"
            continue
        if in_metadata:
            m = re.match(r"\s+class:\s*[\"']?(public|private)[\"']?\s*$", line)
            if m:
                return m.group(1)
    return None


def squash(text):
    return " ".join(text.split())


def has_segments(path, private):
    return f"/{private}/" in f"/{path.as_posix()}/"


def skills_in(root):
    # Third-party packages (the agent runtime's node_modules) carry their own example skills, which Heimdall never loads.
    return sorted(p for p in root.rglob("SKILL.md")
                  if ".git" not in p.parts and "target" not in p.parts and "node_modules" not in p.parts)


def word(text):
    """`text` as a pattern that matches it as whole words, in any case: `Sam` never matches `Samson`."""
    left = r"(?<![A-Za-z0-9])" if text[:1].isalnum() else ""
    right = r"(?![A-Za-z0-9])" if text[-1:].isalnum() else ""
    return re.compile(left + re.escape(text) + right, re.IGNORECASE)


def private_material(dirs):
    """(strings, fingerprint lines) from each private folder. A string is (its number in the file, its pattern,
    whether it holds everywhere or only in the prompt and skills); the number names it in a finding, so the report
    never prints the string itself."""
    strings, lines = [], set()
    for d in dirs:
        listing = d / "private-strings.txt"
        if listing.is_file():
            everywhere, number = True, 0
            for raw in listing.read_text().splitlines():
                raw = raw.strip()
                if not raw or raw.startswith("#"):
                    continue
                if raw == "[everywhere]":
                    everywhere = True
                elif raw == "[prompt and skills]":
                    everywhere = False
                else:
                    number += 1
                    strings.append((number, word(raw), everywhere))
        for md in sorted(d.rglob("*.md")):
            text = md.read_text(errors="replace")
            # The top of environment.md down to its second marker is the app's own template and generated block.
            marker = "<!-- heimdall:environment:end -->"
            if marker in text:
                text = text.split(marker, 1)[1]
            body = text
            front = frontmatter(text)
            if front is not None:
                body = text[len(front) + 8:]
            for line in body.splitlines():
                line = squash(line.strip().lstrip("-*>#|0123456789. ").rstrip("|"))
                if len(line) >= FINGERPRINT_MIN:
                    lines.add(line)
    return strings, sorted(lines)


def check(root, private_dirs=(), whole_tree=False):
    """Every problem found under `root`, as strings. `whole_tree` searches all its text files for private text and
    refuses every private path; otherwise only the public layer is read (a source checkout keeps `lab/` and the
    personal edition's own folders)."""
    root = Path(root)
    problems = []
    strings, fingerprints = private_material([Path(d) for d in private_dirs])

    # 1 and 2: every skill declares a class, and none in a public place is private.
    sets = [root / s for s in PUBLIC_SKILL_SETS] if not whole_tree else [root]
    public_files = []
    for base in sets:
        if not base.is_dir():
            continue
        for skill in skills_in(base):
            rel = skill.relative_to(root)
            declared = skill_class(skill.read_text(errors="replace"))
            if declared is None:
                problems.append(f"{rel}: no class (add `metadata:` / `  class: public` to its frontmatter)")
            elif declared == "private":
                problems.append(f"{rel}: a private-class skill in a public place")
            public_files.extend(p for p in sorted(skill.parent.rglob("*")) if p.is_file())
    skills_root = root / "skills"
    if skills_root.is_dir():
        for entry in sorted(skills_root.iterdir()):
            if entry.is_dir() and f"skills/{entry.name}" not in PUBLIC_SKILL_SETS:
                problems.append(f"skills/{entry.name}/: not a public skill set ({', '.join(PUBLIC_SKILL_SETS)})")

    # 3: private paths. A source checkout legitimately has lab/ and prompt/personal; an exported tree has none.
    if whole_tree:
        seen = set()
        for p in root.rglob("*"):
            if ".git" in p.parts:
                continue
            rel = p.relative_to(root)
            for private in PRIVATE_PATHS:
                if has_segments(rel, private) and private not in seen:
                    seen.add(private)
                    problems.append(f"private path in the tree: {rel} ({private}/)")

    # 4: private strings in the public layer.
    public_files.extend(root / p for p in PUBLIC_PROMPTS if (root / p).is_file())
    for d in APP_PROMPT_DIRS:
        if (root / d).is_dir():
            public_files.extend(sorted((root / d).rglob("*.md")))
    if not whole_tree:
        missing = [p for p in PUBLIC_PROMPTS if not (root / p).is_file()]
        problems.extend(f"{p}: the general prompt file is missing" for p in missing)
    layer_files = sorted({p for p in public_files if p.suffix in TEXT_SUFFIXES})
    for path in layer_files:
        text = path.read_text(errors="replace")
        rel = path.relative_to(root)
        # The app's model prompts are judged by the private strings and lines only: their worked examples carry
        # placeholder paths (`/Users/me/`), model names and dated design notes by design.
        app_prompt = any(has_segments(rel, d) for d in APP_PROMPT_DIRS)
        for n, line in enumerate(text.splitlines(), 1):
            for number, pattern, _ in strings:
                if pattern.search(line):
                    problems.append(f"{rel}:{n}: private string #{number} of private-strings.txt")
            if app_prompt:
                continue
            scrubbed = line
            for ok in ALLOWED_LAYER:
                scrubbed = scrubbed.replace(ok, "")
            for name, pattern in LAYER_PATTERNS:
                if pattern.search(scrubbed):
                    problems.append(f"{rel}:{n}: {name}")
            if MODEL_NAMED.search(scrubbed) and not rel.as_posix().endswith(MODEL_NAMES_ALLOWED_IN):
                problems.append(f"{rel}:{n}: model named")

    # 5: lines of private files, and the everywhere strings, in public files.
    searched = layer_files
    if whole_tree:
        searched = sorted(p for p in root.rglob("*") if p.is_file() and ".git" not in p.parts and p.suffix in TEXT_SUFFIXES
                          and p.stat().st_size < 4_000_000)
    if fingerprints or strings:
        for path in searched:
            try:
                text = path.read_text(errors="replace")
            except OSError:
                continue
            flat = squash(text)
            rel = path.relative_to(root)
            hits = [f for f in fingerprints if f in flat]
            for f in hits[:5]:
                problems.append(f"{rel}: carries a line of a private file ({len(f)} characters, starts {f[:24]!r}…)")
            if len(hits) > 5:
                problems.append(f"{rel}: and {len(hits) - 5} more private lines")
            if whole_tree and path not in layer_files:
                for number, pattern, everywhere in strings:
                    if everywhere and pattern.search(flat):
                        problems.append(f"{rel}: private string #{number} of private-strings.txt")
    return problems


def listing(root):
    root = Path(root)
    files = [p for p in PUBLIC_PROMPTS if (root / p).is_file()]
    for d in APP_PROMPT_DIRS:
        files.extend(str(p.relative_to(root)) for p in sorted((root / d).rglob("*.md")) if (root / d).is_dir())
    for base in PUBLIC_SKILL_SETS:
        files.extend(str(p.relative_to(root)) for p in sorted((root / base).rglob("*")) if p.is_file())
    return files


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--tree", type=Path, help="an exported tree or an app bundle to check as a whole")
    ap.add_argument("--private", type=Path, action="append", default=[], help="a private folder whose text must not be public")
    ap.add_argument("--list", action="store_true", help="print the prompt and skill files a public export may take")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    root = Path(__file__).resolve().parent.parent
    if a.list:
        print("\n".join(listing(root)))
        return
    for d in a.private:
        if not d.is_dir():
            sys.exit(f"PUBLIC SPLIT: private folder not found: {d}")
    problems = check(a.tree or root, a.private, whole_tree=a.tree is not None)
    if a.json:
        print(json.dumps({"tree": str(a.tree or root), "private": [str(d) for d in a.private], "problems": problems}, indent=1))
    if problems:
        print("PUBLIC SPLIT REFUSED:\n  " + "\n  ".join(problems[:60]), file=sys.stderr)
        if len(problems) > 60:
            print(f"  … and {len(problems) - 60} more", file=sys.stderr)
        sys.exit(1)
    if not a.json:
        print(f"public split: clean ({a.tree or root})")


if __name__ == "__main__":
    main()
