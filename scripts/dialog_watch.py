#!/usr/bin/env python3
"""Watch for system dialogs during an unattended run (cleanup night 14 Nov).

13 Nov 22:45 a lane ran a bare Heimdall binary; macOS asked for the login Keychain password in a SecurityAgent dialog
that stood all night, nobody saw it until 23:13, and the installed Heimdall stalled behind it. This watcher sees such a
dialog within 20 s:

  scripts/dialog_watch.py --check              can this process read the dialogs? (run it before User sleeps: the
                                               first run may raise macOS's own Accessibility/Automation prompt)
  scripts/dialog_watch.py --once               one look, recorded, exit
  scripts/dialog_watch.py [--hours 12]         every 20 s until stopped (hard lifetime: --hours, at most 24)

Every --interval seconds it reads the windows of SecurityAgent (Keychain and password prompts), UserNotificationCenter
(privacy and notification alerts) and CoreServicesUIAgent (Gatekeeper alerts) through System Events, read-only, and
records each new dialog once into the status file (default ~/Work/Assistant/night-pass-status.md, appended): the time,
the window's text, the requesting process and what was done.

It presses a button only on a dialog whose requester is a process our own task started: every running process with
that name runs from a worktree or target/ folder in ~/Work/Projects or a task's scratch folder (/tmp/claude-…,
/tmp/Heimdall…), and none from /Applications or ~/Applications. The press re-reads the dialogs first and presses only
when exactly one window still shows the same text and buttons.
Then it presses Deny, Don't Allow or Cancel, never Allow, Always Allow, OK or Open. A dialog for the installed Heimdall,
any other app or an unknown requester is only recorded, for the morning.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

WATCHED = ("SecurityAgent", "UserNotificationCenter", "CoreServicesUIAgent")
# Buttons that refuse. Never "Allow", "Always Allow", "OK", "Open" or anything else.
SAFE_BUTTONS = ("Deny", "Don't Allow", "Don’t Allow", "Cancel")
# Where processes our own tasks start live: a build or worktree inside the Vault's projects, or a task's scratch or lab
# folder. Not any `target/` on the Mac (other projects' dev builds) and not /private/var/folders (App Translocation runs
# downloaded apps from there; review 14 Nov).
OURS_IN_VAULT = ("/.claude/worktrees/", "/target/")
OURS_SCRATCH = ("/private/tmp/claude-", "/tmp/claude-", "/private/tmp/Heimdall", "/tmp/Heimdall")


def ours_path(p: str, home: str) -> bool:
    vault = os.path.join(home, "Work/Projects") + "/"
    return (p.startswith(vault) and any(m in p for m in OURS_IN_VAULT)) or p.startswith(OURS_SCRATCH)
INTERVAL_S = 20
MAX_HOURS = 24
STATUS = Path.home() / "Work/Assistant/night-pass-status.md"

Q = "[\"“”']"  # straight or curly quotes
PATTERNS = [
    re.compile(rf"^{Q}?(?P<app>.+?){Q}? wants to use your confidential information stored in {Q}?(?P<item>.+?){Q}? in your keychain"),
    re.compile(rf"^{Q}?(?P<app>.+?){Q}? wants to access key {Q}?(?P<item>.+?){Q}? in your keychain"),
    re.compile(rf"^{Q}?(?P<app>.+?){Q}? wants to use the {Q}(?P<item>.+?){Q} keychain"),
    re.compile(rf"^{Q}?(?P<app>.+?){Q}? wants to (?:make changes|use |access |sign |export |modify |add )"),
    re.compile(rf"^{Q}(?P<app>.+?){Q} would like to"),
    re.compile(rf"^{Q}(?P<app>.+?){Q} (?:is an app downloaded|cannot be opened|can’t be opened|can't be opened|is damaged|will damage)"),
    re.compile(rf"^Do you want the application {Q}(?P<app>.+?){Q} to accept incoming network connections"),
]


@dataclass
class Window:
    process: str
    title: str = ""
    texts: list[str] = field(default_factory=list)
    buttons: list[str] = field(default_factory=list)
    index: int = 1  # the window's index in its process (System Events, 1-based)

    @property
    def text(self) -> str:
        return " ".join(t.strip() for t in [self.title, *self.texts] if t and t.strip())

    def signature(self) -> str:
        return hashlib.sha256(f"{self.process}\0{self.text}\0{'|'.join(self.buttons)}".encode()).hexdigest()[:16]


def requester(w: Window) -> tuple[str | None, str | None]:
    """The requesting app's name and the Keychain item, from the window's text (None when it names none)."""
    for line in [w.title, *w.texts]:
        for p in PATTERNS:
            m = p.search((line or "").strip())
            if m:
                return m.group("app").strip(), (m.groupdict().get("item") or None)
    return None, None


def classify(name: str | None, processes: list[tuple[int, str]], home: str) -> tuple[bool, str]:
    """(ours, why). Ours only when every running process with that executable name runs from a worktree, a target/
    folder or a temporary folder, and none from an Applications folder."""
    if not name:
        return False, "the dialog names no requester"
    paths = [p for _, p in processes if os.path.basename(p) == name]
    if not paths:
        return False, f"no running process named {name}"
    for p in paths:
        if p.startswith("/Applications/") or p.startswith(os.path.join(home, "Applications") + "/"):
            return False, f"{name} runs from {p}"
    for p in paths:
        if not ours_path(p, home):
            return False, f"{name} runs from {p}"
    return True, paths[0]


def safe_button(buttons: list[str]) -> str | None:
    for b in SAFE_BUTTONS:
        if b in buttons:
            return b
    return None


# ---- System Events (read-only, plus one press of a refusing button) -------------------------------------------------

READ_JXA = r"""
function run(argv) {
  const se = Application('System Events');
  const out = [];
  for (const name of argv) {
    let procs = [];
    try { procs = se.processes.whose({name: name})(); } catch (e) { continue; }
    for (const p of procs) {
      let wins = [];
      try { wins = p.windows(); } catch (e) { continue; }
      wins.forEach((w, i) => {
        const texts = [], buttons = [];
        let title = '';
        try { title = w.name() || ''; } catch (e) {}
        try {
          w.entireContents().forEach(el => {
            try {
              const c = el.class();
              if (c === 'staticText' || c === 'textArea') { const v = el.value(); if (v) texts.push(String(v)); }
              else if (c === 'button') { const n = el.name(); if (n) buttons.push(String(n)); }
            } catch (e) {}
          });
        } catch (e) {}
        out.push({process: name, index: i + 1, title: String(title), texts: texts, buttons: buttons});
      });
    }
  }
  return JSON.stringify(out);
}
"""

PRESS_JXA = r"""
function run(argv) {
  // Re-read every window of every process with this name and press only on the one that still shows exactly the
  // text and buttons that were classified: a window index can point at another dialog by now (review 14 Nov).
  const [name, expected, button] = argv;
  const se = Application('System Events');
  const matches = [];
  for (const p of se.processes.whose({name: name})()) {
    let wins = [];
    try { wins = p.windows(); } catch (e) { continue; }
    for (const w of wins) {
      const texts = [], buttons = [], found = [];
      let title = '';
      try { title = w.name() || ''; } catch (e) {}
      try {
        w.entireContents().forEach(el => {
          try {
            const c = el.class();
            if (c === 'staticText' || c === 'textArea') { const v = el.value(); if (v) texts.push(String(v)); }
            else if (c === 'button') { const n = el.name(); if (n) { buttons.push(String(n)); if (String(n) === button) found.push(el); } }
          } catch (e) {}
        });
      } catch (e) {}
      if (JSON.stringify({title: String(title), texts: texts, buttons: buttons}) === expected && found.length > 0) {
        matches.push(found[0]);
      }
    }
  }
  if (matches.length !== 1) { throw new Error(matches.length + ' windows match the dialog; nothing pressed'); }
  matches[0].click();
  return 'pressed';
}
"""


def osascript(script: str, args: list[str], timeout: float = 15) -> str:
    r = subprocess.run(["osascript", "-l", "JavaScript", "-e", script, *args], capture_output=True, text=True, timeout=timeout)
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip() or f"osascript exited {r.returncode}")
    return r.stdout.strip()


def read_windows() -> list[Window]:
    return [Window(**w) for w in json.loads(osascript(READ_JXA, list(WATCHED)) or "[]")]


def press(w: Window, button: str) -> None:
    assert button in SAFE_BUTTONS, button
    osascript(PRESS_JXA, [w.process, expected(w), button])


def expected(w: Window) -> str:
    """The window as the press script re-reads it (same keys, order and separators as JSON.stringify)."""
    return json.dumps({"title": w.title, "texts": w.texts, "buttons": w.buttons}, separators=(",", ":"), ensure_ascii=False)


def processes() -> list[tuple[int, str]]:
    out = subprocess.run(["ps", "-axww", "-o", "pid=,comm="], capture_output=True, text=True, timeout=10).stdout
    rows = []
    for line in out.splitlines():
        pid, _, path = line.strip().partition(" ")
        if pid.isdigit():
            rows.append((int(pid), path.strip()))
    return rows


# ---- The watch -------------------------------------------------------------------------------------------------------

def stamp() -> str:
    return time.strftime("%Y-%m-%d %H:%M:%S")


def record(status: Path, line: str) -> None:
    status.parent.mkdir(parents=True, exist_ok=True)
    with status.open("a", encoding="utf-8") as f:
        f.write(line + "\n")
    print(line, flush=True)


def handle(w: Window, procs: list[tuple[int, str]], home: str, dismiss: bool, presser=press) -> str:
    """One new dialog: what it is and what was done, as one status line."""
    name, item = requester(w)
    ours, why = classify(name, procs, home)
    action = "left for the morning"
    if ours and dismiss:
        button = safe_button(w.buttons)
        if button is None:
            action = f"ours, but no Deny/Cancel button ({', '.join(w.buttons) or 'none'}); left for the morning"
        else:
            try:
                presser(w, button)
                action = f"pressed {button}"
            except Exception as e:  # noqa: BLE001
                action = f"pressing {button} failed ({e}); left for the morning"
    who = f"requester {name}" if name else "no requester named"
    whose = f"ours: {why}" if ours else f"not ours: {why}"
    what = f" · item {item}" if item else ""
    return f"- {stamp()} · dialog · {w.process} · \"{w.text[:300]}\" · {who} ({whose}){what} · {action}"


def watch(status: Path, interval: float, hours: float, dismiss: bool, once: bool,
          reader=read_windows, lister=processes, presser=press, sleep=time.sleep, now=time.monotonic,
          home: str | None = None) -> int:
    home = home or str(Path.home())
    end = now() + min(hours, MAX_HOURS) * 3600
    showing: set[str] = set()
    failing = False
    while True:
        try:
            windows = reader()
            if failing:
                record(status, f"- {stamp()} · dialog watch reads dialogs again")
                failing = False
        except Exception as e:  # noqa: BLE001
            if not failing:
                record(status, f"- {stamp()} · dialog watch cannot read dialogs ({str(e)[:200]}); dialogs go unseen until it can")
                failing = True
            windows = None
        if windows is not None:
            current = {w.signature(): w for w in windows if w.text or w.buttons}
            fresh = [w for s, w in current.items() if s not in showing]
            if fresh:
                procs = lister()
                for w in fresh:
                    record(status, handle(w, procs, home, dismiss, presser))
            # A dialog is recorded once while it stands; one that comes back later is recorded again.
            showing = set(current)
        if once or now() >= end:
            return 1 if failing else 0
        sleep(interval)


def main(argv: list[str] | None = None) -> int:
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--status", type=Path, default=STATUS, help="status file to append to")
    a.add_argument("--interval", type=float, default=INTERVAL_S, help="seconds between looks (at most 60)")
    a.add_argument("--hours", type=float, default=12, help=f"hard lifetime in hours (at most {MAX_HOURS})")
    a.add_argument("--no-dismiss", action="store_true", help="record only; never press anything")
    a.add_argument("--once", action="store_true", help="one look, then exit")
    a.add_argument("--check", action="store_true", help="can this process read the dialogs? prints what it sees")
    o = a.parse_args(argv)
    if o.check:
        try:
            windows = read_windows()
        except Exception as e:  # noqa: BLE001
            print(f"cannot read dialogs: {e}", file=sys.stderr)
            return 1
        print(f"reads dialogs: {len(windows)} window(s) open in {', '.join(WATCHED)}")
        for w in windows:
            print(f"  {w.process}: \"{w.text[:200]}\" buttons {w.buttons}")
        return 0
    return watch(o.status, min(max(o.interval, 1), 60), o.hours, not o.no_dismiss, o.once)


if __name__ == "__main__":
    sys.exit(main())
