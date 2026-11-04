#!/usr/bin/env python3
"""CLI parity: the same commands through the Swift `islands` and the Rust one, each in its own isolated data folder.

    python3 -I crates/islands/tests/cli_parity.py [SWIFT_BIN] [RUST_BIN]   (or ISLANDS_SWIFT_BIN)

Goals (31 Oct): the Swift helper is the rollback and predates Goals (its default list is To do, due items move, put/rm
touch plan lists). Every step that touches the plan lists or the help text runs on Rust alone, checked only for a clean
exit (0 or 2, no panic), with a printed note; the rest compares both. Compares, step by step: exit code, stdout and stderr (data-folder paths and timestamps normalised), then every island
file (parsed, `started`/`updated`/`ended` ignored; also byte for byte after the same normalisation) and the pending
responses (parsed, `at` and file names ignored; a `due` to the nearest 10 s, as relative times are read a few ms
apart). Skips (exit 0) when the Swift binary is missing. Data: islands/lab/data/cli-parity-*
(git-ignored), removed at the end unless KEEP=1.
"""
import json
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
SWIFT = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("ISLANDS_SWIFT_BIN", "")
RUST = sys.argv[2] if len(sys.argv) > 2 else os.path.join(REPO, "target", "debug", "islands")
LAB = os.path.join(REPO, "islands", "lab", "data")
DIRS = {"swift": os.path.join(LAB, "cli-parity-swift"), "rust": os.path.join(LAB, "cli-parity-rust")}
WORK = os.path.join(LAB, "cli-parity-work")
TS = ("started", "updated", "ended")

if not os.path.exists(SWIFT):
    print(f"skip: no Swift build at {SWIFT}")
    sys.exit(0)
if not os.path.exists(RUST):
    print(f"no Rust build at {RUST} (cargo build -p islands --bin islands)")
    sys.exit(1)


# Fixtures and folders ------------------------------------------------------------------------------------------------

def fresh():
    for d in list(DIRS.values()) + [WORK]:
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
    os.makedirs(os.path.join(WORK, "sub"))
    for name, data in [("notes.md", b"# Notes\n"), (".hidden", b"x"), ("a.tar.gz", b"\x00\x01"), ("img.png", b"\x89PNG"),
                       ("dot.", b"x"), ("two words.final draft.txt", b"x"), ("Makefile", b"all:\n")]:
        with open(os.path.join(WORK, name), "wb") as f:
            f.write(data)


def write_both(rel, content):
    """The same file in both data folders (a dict/list is written as JSON, a str as is)."""
    for d in DIRS.values():
        p = os.path.join(d, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(content if isinstance(content, str) else json.dumps(content))


# Normalising ---------------------------------------------------------------------------------------------------------

def norm_out(s, d, which):
    s = s.replace(d, "<ROOT>")
    # The one put help line Goals reworded (the Swift rollback predates Goals).
    s = s.replace("default right, To do left", "default right, plan lists left")
    # Timestamps go entirely (Rust's `view` stamps `updated` too, new tonight).
    s = re.sub(r'"(?:started|updated|ended)":-?[0-9][0-9.e+-]*,?', "", s)
    s = s.replace(",}", "}")
    s = re.sub(r'"at":"[^"]*"', '"at":T', s)
    return s


def norm_bytes(s):
    s = re.sub(r'^(\s*"(?:started|updated|ended)" : ).*?(,?)$', r"\1T\2", s, flags=re.M)
    s = re.sub(r'^(\s*"due" : )([0-9.e+]+)(,?)$', lambda m: f"{m.group(1)}{round(float(m.group(2)) / 10)}{m.group(3)}", s, flags=re.M)
    return s


def drop_ts_lines(s):
    s = re.sub(r'^\s*"(?:started|updated|ended)" : T,?\n', "", s, flags=re.M)
    s = re.sub(r",\n(\s*[}\]])", r"\n\1", s)
    return s


def snapshot(d):
    isl, resp, raw = {}, [], {}
    idir, rdir = os.path.join(d, "islands"), os.path.join(d, "responses")
    for n in sorted(os.listdir(idir)) if os.path.isdir(idir) else []:
        p = os.path.join(idir, n)
        if n.startswith(".") or not os.path.isfile(p):
            isl[n] = "<entry>"
            continue
        b = open(p, "rb").read().decode("utf-8", "replace")
        raw[n] = b
        try:
            o = json.loads(b)
            if isinstance(o, dict):
                for k in TS:
                    o.pop(k, None)
                for it in o.get("items") or []:
                    if isinstance(it, dict) and isinstance(it.get("due"), (int, float)):
                        it["due"] = round(it["due"] / 10)
            isl[n] = o
        except ValueError:
            isl[n] = "<unparsed>" + b
    for n in sorted(os.listdir(rdir)) if os.path.isdir(rdir) else []:
        try:
            o = json.loads(open(os.path.join(rdir, n)).read())
            if isinstance(o, dict):
                o.pop("at", None)
            resp.append(json.dumps(o, sort_keys=True))
        except Exception:
            resp.append("<file " + n + ">")
    return isl, sorted(resp), raw


# Running -------------------------------------------------------------------------------------------------------------

results = {"steps": 0, "same": 0, "diff": [], "byte_diff": [], "rust_only": 0, "rust_bad": []}
HELP = ([], ["help"], ["--help"], ["-h"], ["bogus"])


def rust_only(args, stdin=None):
    """A step the Swift rollback cannot match (it predates Goals): Rust alone, a clean exit and no panic."""
    env = dict(os.environ, FLOATING_ISLANDS_DIR=DIRS["rust"])
    p = subprocess.run([RUST] + args, input=(stdin or "").encode(), capture_output=True, cwd=WORK, env=env, timeout=30)
    results["rust_only"] += 1
    err = p.stderr.decode()
    if p.returncode not in (0, 2) or "panicked" in err:
        results["rust_bad"].append((" ".join(args), p.returncode, err[-400:]))


def step(args, stdin=None):
    """Compared on both, unless it touches the plan lists or the help (Goals; Swift is the rollback)."""
    plan = args[:1] in (["plan"], ["todo"]) or (args[:1] in (["rm"], ["put"]) and any(a in ("goals", "scheduled", "settled") for a in args[1:2]))
    if plan or list(args) in [list(h) for h in HELP] or args[-1:] in (["--help"], ["-h"]):
        rust_only(args, stdin)
    else:
        run(args, stdin)


def run(args, stdin=None, label=None):
    out = {}
    for which, binary in (("swift", SWIFT), ("rust", RUST)):
        env = dict(os.environ, FLOATING_ISLANDS_DIR=DIRS[which])
        p = subprocess.run([binary] + args, input=(stdin or "").encode(), capture_output=True, cwd=WORK, env=env, timeout=30)
        out[which] = (p.returncode, norm_out(p.stdout.decode(), DIRS[which], which), norm_out(p.stderr.decode(), DIRS[which], which))
    results["steps"] += 1
    problems = []
    for k, name in enumerate(("exit", "stdout", "stderr")):
        if out["swift"][k] != out["rust"][k]:
            problems.append(f"  {name}:\n    swift: {out['swift'][k]!r}\n    rust:  {out['rust'][k]!r}")
    (si, sr, sraw), (ri, rr, rraw) = snapshot(DIRS["swift"]), snapshot(DIRS["rust"])
    if si != ri:
        for n in sorted(set(si) | set(ri)):
            if si.get(n) != ri.get(n):
                problems.append(f"  file {n}:\n    swift: {json.dumps(si.get(n), sort_keys=True)}\n    rust:  {json.dumps(ri.get(n), sort_keys=True)}")
    if sr != rr:
        problems.append(f"  responses:\n    swift: {sr}\n    rust:  {rr}")
    for n in sorted(set(sraw) & set(rraw)):
        a, b = drop_ts_lines(norm_bytes(sraw[n])), drop_ts_lines(norm_bytes(rraw[n]))
        if a != b and (n, label) not in results["byte_diff"]:
            results["byte_diff"].append((n, " ".join(args)))
    if problems:
        results["diff"].append((" ".join(args), problems))
    else:
        results["same"] += 1


def phase(title):
    print(f"-- {title}")


# The sequence --------------------------------------------------------------------------------------------------------

fresh()

phase("help and empty folder")
for a in [[], ["help"], ["--help"], ["-h"], ["bogus"], ["dir"], ["put", "--help"], ["put", "x", "-h"], ["plan", "--help"],
          ["todo", "-h"], ["plan", "scheduled", "--help"], ["list"], ["list", "--json"], ["state"], ["state", "--json"],
          ["state", "x"], ["state", "x", "--json"], ["responses"], ["responses", "--json"], ["quit"], ["rm"], ["rm", "--all"],
          ["rm", "nope"], ["clear"], ["plan"], ["plan", "list"], ["todo"], ["plan", "scheduled"]]:
    step(a)

phase("put: style checks and option errors")
for a in [["put"], ["put", "--title", "X"], ["put", "a"], ["put", "a", "--title", ""], ["put", "img-job", "--title", "img job"],
          ["put", "img_job", "--title", "img-job"], ["put", "a", "--title", "One two three four"],
          ["put", "a", "--title", "Good", "--stage", "one two three"], ["put", "a", "--title", "Good", "--summary", "two\nlines"],
          ["put", "a", "--title", "Good", "--summary", "x" * 111], ["put", "a", "--title", "Good", "--summary", "é" * 110],
          ["put", "a", "--title", "Good", "--summary", "\U0001F44D\U0001F3FD" * 111],
          ["put", "a", "--title"], ["put", "a", "--preset", "bogus"], ["put", "a", "--state", "asleep"],
          ["put", "a", "--side", "middle"], ["put", "a", "--progress", "lots"], ["put", "a", "--at", "1"],
          ["put", "a", "--at", "x,y"], ["put", "a", "--order", "first"], ["put", "a", "--kv", "novalue"],
          ["put", "a", "--parts", "{\"a\":1}"], ["put", "a", "--parts", "nope"], ["put", "a", "--json", "[1]"],
          ["put", "a", "--json", "{bad"], ["put", "a", "--json", "x" * 120], ["put", "a", "--bogus"], ["put", "a", "stray"],
          ["put", "a", "--title", "Good", "--json", "{\"preset\":\"bogus\"}"]]:
    step(a)

phase("put: every option")
run(["put", "img", "--title", "Qwen-Image", "--state", "running", "--stage", "Downloading", "--icon", "photo",
     "--summary", "20 GB of weights, then convert and install", "--step", "Download=now", "--step", "Convert",
     "--step", "Install = done", "--step", "a=b", "--item", "A=running", "--item", "B", "--item", "C=done", "--item", "D=waiting",
     "--item", "Arsenal: a=b", "--kv", " Size = 20 GB ", "--kv", "Rate=5\t", "--progress", "0.425", "--image", "img.png",
     "--open", "example.com", "--value", "412", "--preset", "task", "--expanded", "--from-notch", "--order", "2"])
run(["put", "img", "--step", "none", "--item", "none", "--kv", "none"])
run(["put", "img", "--step", "", "--item", "X", "--item", "none", "--kv", "A=1", "--kv", "B=2"])
run(["put", "img", "--progress", "none"])
run(["put", "img", "--progress", "1.7"])
run(["put", "img", "--progress", "0.999"])
run(["put", "img", "--progress", "1e-1"])
run(["put", "img", "--side", "top"])
run(["put", "img", "--at", "10, 20"])
run(["put", "img", "--at", "1,,2"])
run(["put", "img", "--at", "1.5,x,2.25"])
run(["put", "img", "--at", "10.5,20.25", "--side", "left"])
run(["put", "img", "--auto"])
run(["put", "img", "--order", "3"])
run(["put", "img", "--collapsed", "--summary", "", "--image", "", "--open", ""])
for t in ["https://www.example.com/a b", "mailto:a@b.c", "/abs//path/", "~/x", "notes.md", "foo", "sub//../k/", "a.b", "~"]:
    run(["put", "img", "--open", t])
for t in ["/tmp//x/../y/", "rel/./z/", "~", "img.png", "~/a//b"]:
    run(["put", "img", "--image", t])
run(["put", "img", "--parts", "[{\"type\":\"button\",\"label\":\"A\",\"action\":\"a\"},{\"type\":\"slider\",\"value\":0.5}]"])
run(["put", "img", "--json", "{\"title\":\"J son\",\"extra\":{\"n\":1.5,\"z\":[1,2.0,\"a/b\"]},\"progress\":null,\"value\":3.14159}"])
run(["put", "img", "-"], stdin="{\"value\": 7, \"stage\": \"Piped\", \"icon\": null}")
run(["put", "img", "--state", "done"])
run(["put", "img", "--state", "done", "--value", "Finished"])
run(["put", "img", "--state", "failed"])
run(["put", "img", "--state", "running", "--preset", "waiting"])
run(["put", "img", "--title", "Qwen Image Two"])
run(["put", "img", "--title", "img"])
run(["put", "v1", "--preset", "viewer"])
run(["put", "n1", "--title", "A note", "--preset", "note", "--value", "x", "--state", "waiting"])
run(["put", "a/b c", "--title", "Weird id"])
run(["put", "big", "--title", "Numbers", "--json", "{\"a\":1e17,\"c\":1.5e-7,\"d\":9007199254740993,\"B\":1,\"_\":2,\"10\":3,\"9\":4,\"aB\":5,\"ab\":6,\"é\":7,\"s\":\"\\b\\f\\r\\u0001\\u007f\"}"])
write_both("islands/pinned.json", {"title": "Pinned", "edgeY": 157, "side": "left", "progress": 0.5, "state": "running"})
run(["put", "pinned", "--stage", "Going"])
run(["put", "pinned", "--order", "1"])

phase("move")
write_both("islands/pinned.json", {"title": "Pinned", "edgeY": 157, "side": "left"})
for a in [["move"], ["move", "img"], ["move", "nope", "1", "2"], ["move", "nope", "--side", "bogus"], ["move", "pinned", "--auto"],
          ["move", "img", "--side"], ["move", "img", "--side", "bogus"], ["move", "img", "--side", "left"],
          ["move", "img", "10", "20.5"], ["move", "img", "x", "y"], ["move", "img", "5"], ["move", "img", "1e2", "-3"],
          ["move", "img", "--side", "top"], ["move", "img", "--auto"]]:
    step(a)

phase("view")
for a in [["view"], ["view", "--id", "x"], ["view", "missing.md"], ["view", "notes.md"],
          ["view", "./sub/../notes.md", "--title", "Notes", "--action", "Do all=do-all", "--action", "Skip", "--action", "=x", "--action"],
          ["view", "notes.md"], ["view", "notes.md", "--action", "Again"], ["view", "notes.md", "--id", "v2"], ["view", "notes.md", "--title"],
          ["view", ".hidden"], ["view", "a.tar.gz"], ["view", "dot."], ["view", "two words.final draft.txt"], ["view", "sub"],
          ["view", "sub/"], ["view", WORK + "//Makefile"], ["view", "Makefile", "--id"]]:
    step(a)

phase("open, close, hide, show")
for a in [["open"], ["open", "nope"], ["open", "img", "pinned"], ["close", "img"], ["expand", "img"], ["collapse", "img"],
          ["pin", "img"], ["unpin", "img"], ["hide", "img"], ["show", "img"], ["hide", "img", "nope", "pinned"]]:
    step(a)

phase("state and list")
write_both("state.json", {"islands": {"img": {"open": True, "x": 10.4, "y": 20.6, "width": 300, "height": 100.5},
                                      "pinned": {"open": 1, "x": "str", "width": 2.5}, "zz": 5, "Big": {"open": False, "x": -3.5}}})
for a in [["state"], ["state", "img"], ["state", "zz"], ["state", "--json"], ["state", "img", "--json"], ["state", "none", "--json"],
          ["state", "--json", "pinned"], ["list"], ["list", "--json"]]:
    step(a)

phase("responses and quit")
write_both("responses/img-1-1-1.json", {"island": "img", "kind": "button", "action": "go", "at": "2026-10-23T08:00:00Z"})
write_both("responses/img-2-1-1.json", {"island": "img", "kind": "slider", "value": 0.25})
write_both("responses/x-3-1-1.json", {"island": 5, "action": None, "value": True})
write_both("responses/y-4-1-1.json", {"kind": "list", "value": "row"})
write_both("responses/notes.txt", "not json")
write_both("responses/.hidden.json", {"island": "h"})
write_both("responses/broken.json", "{")
for a in [["responses"], ["responses", "--json"], ["responses", "--consume"], ["responses"]]:
    step(a)
write_both(".app.pid", "999999\n")
run(["quit"])
write_both(".app.pid", "soon")
run(["quit"])

phase("rm")
for a in [["rm", "nope"], ["rm", "a/b c"], ["rm", "v1", "n1", "v1"], ["rm", "--all", "img"], ["list"]]:
    step(a)
run(["rm", "--all"])
run(["clear"])

phase("plan: add and list (Rust alone from here: the plan lists are Goals, Scheduled, Settled)")
for a in [["plan", "add"], ["plan", "bogus"], ["plan", "goals", "bogus", "x"], ["plan", "add", "x", "--when"], ["plan", "--id"],
          ["plan", "list", "--to", "x"], ["plan", "goals", "add", "Echo bug"], ["plan", "add", "Port the CLI", "--running"],
          ["plan", "add", "Approve caches", "--proposal", "Delete 12 GB"], ["plan", "add", "Ask", "--proposal", "Go?", "--running"],
          ["plan", "add", "A", "B", "", "--waiting"], ["plan", "scheduled", "add", "Push branch", "--when", "Tonight", "--detail", "after tests"],
          ["plan", "scheduled", "add", "Plain later"], ["plan", "scheduled", "add", "Grouped later", "--waiting"],
          ["plan", "settled", "add", "Done thing"], ["plan", "goals", "add", "Same", "--item-id", "fixed"],
          ["plan", "goals", "add", "Echo bug"], ["plan", "add", "!!!"], ["plan", "add", "Zażółć gęślą jaźń " * 4],
          ["plan", "add", "x", "--bogus-flag"], ["plan"], ["plan", "list"], ["plan", "scheduled"], ["plan", "goals", "list"],
          ["plan", "--id", "x", "scheduled", "list"], ["todo"], ["todo", "list"]]:
    rust_only(a)

phase("plan: tick, move, remove, prune")
for a in [["plan", "done", "1"], ["plan", "done", "echo-bug"], ["plan", "done", "port"], ["plan", "done", "e"],
          ["plan", "done", "zzz"], ["plan", "done", "99"], ["plan", "done", "0"], ["plan", "done", "-1"], ["plan", "done", "+2"],
          ["plan", "done", ""], ["plan", "undo", "1"], ["plan", "undo", "Echo bug"], ["plan", "done", "ZAŻÓŁĆ"], ["plan"],
          ["plan", "move", "1"], ["plan", "move", "1", "--waiting"], ["plan", "move", "2", "--when", "Fri"],
          ["plan", "move", "2", "3", "--discussed", "--when", "Sat"], ["plan", "move", "fixed", "--to", "scheduled", "--when", "Sun"],
          ["plan", "scheduled", "move", "1", "--to", "goals"], ["plan", "scheduled", "move", "1", "--to", "scheduled"],
          ["plan", "move", "1", "--to", "bogus"], ["plan", "goals", "move", "1", "--to", "goals", "--id", "todo"],
          ["plan", "move", "1", "--to", "settled", "--discussed"], ["plan", "settled", "move", "1", "--to", "goals"],
          ["plan", "scheduled", "move", "Plain", "--running"], ["plan"],
          ["plan", "rm", "1", "1"], ["plan", "rm", "nothing-like-this"], ["plan", "prune"], ["plan", "done", "1", "2"],
          ["plan", "prune"], ["plan", "scheduled", "prune"], ["plan"]]:
    rust_only(a)

phase("plan --due")
for a in [["plan", "scheduled", "add", "Push", "--due", "2026-10-24 10:00"], ["plan", "scheduled", "add", "Iso", "--due", "2026-10-24T10:00"],
          ["plan", "scheduled", "add", "Secs", "--due", "2026-10-24 10:00:30"], ["plan", "scheduled", "add", "Offset", "--due", "2026-10-24T10:00:00+02:00"],
          ["plan", "scheduled", "add", "Zulu", "--due", "2026-12-24T08:00:00Z"], ["plan", "scheduled", "add", "Tomorrow", "--due", "tomorrow 10:00"],
          ["plan", "scheduled", "add", "Today", "--due", "today 23:59"], ["plan", "scheduled", "add", "Tonight", "--due", "Tonight", "--when", "Late"],
          ["plan", "scheduled", "add", "Rel", "--due", "+2h"], ["plan", "scheduled", "add", "Days", "--due", "+3d"],
          ["plan", "scheduled", "add", "Mins", "--due", "+30m"], ["plan", "add", "Goals ask due", "--due", "+10d"],
          ["plan", "scheduled", "add", "Bad", "--due", "Friday"], ["plan", "scheduled", "add", "Bad", "--due", "tomorrow 25:00"],
          ["plan", "scheduled", "add", "Bad", "--due", "+2x"], ["plan", "scheduled", "add", "Bad", "--due", "+-2h"],
          ["plan", "scheduled", "add", "Bad", "--due", "2026-13-01 10:00"], ["plan", "scheduled", "add", "Bad", "--due", "none"],
          ["plan", "scheduled", "add", "Bad", "--due"], ["plan", "scheduled", "list", "--due", "junk"],
          ["plan", "scheduled", "move", "push", "--due", "+5h"], ["plan", "scheduled", "move", "iso", "--due", "+1d", "--when", "Fri"],
          ["plan", "scheduled", "move", "secs", "--due", "+2h", "--to", "goals"], ["plan", "scheduled"], ["plan"], ["todo"]]:
    rust_only(a)
write_both("islands/scheduled.json", {"id": "scheduled", "preset": "todo", "list": "scheduled", "items": [
    {"id": "a", "title": "Both", "when": "Fri", "due": 1792800000}, {"id": "b", "title": "String due", "due": "1792800000"},
    {"id": "c", "title": "Bad due", "due": "soon", "when": "Sat"}, {"id": "d", "title": "Past", "due": 1000}]})
rust_only(["plan", "scheduled"])

phase("old callers' todo (maps silently to Goals) and agents' own checklists")
for a in [["todo", "add", "Thing", "--waiting"], ["todo", "done", "Thing"], ["todo", "move", "Thing", "--running"],
          ["todo", "move", "Thing"], ["todo", "rm", "Thing"], ["todo", "prune"], ["todo", "--id", "mylist", "add", "Other"],
          ["todo", "--id", "mylist"], ["todo", "--id", "mylist", "move", "1", "--to", "scheduled"],
          ["todo", "--id", "mylist", "add", "Back"], ["todo", "--id", "mylist", "move", "1", "--to", "goals"], ["todo"], ["plan"],
          ["todo", "scheduled", "add", "x"]]:
    rust_only(a)
rust_only(["clear"])
write_both("islands/open.json", {"preset": "todo", "title": "Old list", "items": ["Plain", {"title": "Port", "group": "running"},
                                 {"title": "Test", "group": "waiting", "done": True}, {"id": 7, "title": "Numbered"},
                                 {"title": "Ask", "group": "waiting", "proposal": {"text": "Go?"}, "chosen": "yes"},
                                 {"title": "Later", "proposal": "Plain text"}, {"title": ""}, {"group": "x"}]})
write_both("islands/settled.json", {"edgeY": 157, "expanded": False, "icon": "checkmark.seal", "id": "settled",
                                    "items": ["Islands into Heimdall", "Drop replies", "Island closes"], "order": 9,
                                    "side": "left", "stage": "Running", "state": "running", "title": "Scheduled"})
for a in [["todo"], ["todo", "done", "Plain"], ["todo", "done", "7"], ["todo", "add", "New one"], ["todo", "move", "Ask", "--to", "scheduled"],
          ["plan", "settled"], ["plan", "settled", "done", "1"], ["plan", "settled", "add", "More", "--running"], ["plan"], ["list"]]:
    rust_only(a)

# Report --------------------------------------------------------------------------------------------------------------

print(f"\n{results['same']}/{results['steps']} steps identical")
print(f"{results['rust_only']} steps on Rust alone (the Swift rollback predates Goals), {len(results['rust_bad'])} unclean")
for args, code, err in results["rust_bad"]:
    print(f"\nRUST islands {args}: exit {code}\n{err}")
for args, problems in results["diff"]:
    print(f"\nDIFF islands {args}")
    print("\n".join(problems))
if results["byte_diff"]:
    print(f"\nfiles that parse the same but differ in bytes: {len(results['byte_diff'])}")
    for n, args in results["byte_diff"][:20]:
        print(f"  {n} after: islands {args}")
if os.environ.get("KEEP") != "1":
    for d in list(DIRS.values()) + [WORK]:
        shutil.rmtree(d, ignore_errors=True)
sys.exit(1 if results["diff"] or results["rust_bad"] else 0)
