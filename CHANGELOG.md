# Changelog

## 0.1.0-alpha.2 (14 Nov 2026)

What changed since alpha.1, in plain words.

**Works below macOS 27.** In alpha.1 the wake decider's and memory search's GPU code needed macOS 27, so on older
systems Setup never finished. Both are now built for macOS 14.4, the app's stated minimum, and were tested on
macOS 26.6. Below macOS 26, speech recognition runs on its Core ML fallback; macOS 14.4 and 15 were not tested.
Memory search is recommended on macOS 26 or later.

**No password dialogs.** Heimdall reads its saved keys without ever asking macOS to show a Keychain prompt. If a key
cannot be read (for example after an upgrade from alpha.1), a pill says "Saved key unreadable" and asks you to
"re-enter the key in Settings ▸ Providers", instead of the app freezing behind a dialog.

**General system prompt and public skills.** The agent's system prompt and the skills that ship are general text,
the same in every install: no user-specific instructions ship.

**Permissions after an update.** After an update, Heimdall asks again for a permission macOS dropped, or points to
System Settings.

**Better memory search.** Searching what you saw and heard is faster, and a small reranking model (Qwen3-Reranker-0.6B,
downloaded at first run, 1.2 GB) puts the best matches first. If it is missing or the Mac is short of memory, search
keeps working: results keep plain similarity order, or, when meaning search is paused for memory, come from a
search by words and the agent is told so.

**Your name, not the developer's.** Prompts, notices and spoken lines name you by the name you set in
Personalisation, or "the user" until you set one. A test fails if a prompt names the developer.

**Talking over Heimdall.** Speaking over a reply stops it more reliably, including with AirPods. On calls with Listening mode
on, only you, the guest you asked Heimdall to listen to, or a voice it has not named yet can stop it. Playback from the Mac's speakers is no longer mistaken for
you.

**Tasks for coding agents.** Finished tasks close themselves. Approvals from sub-agents reach you, sends always land,
and one card can archive several threads. When the Claude Code subscription route fails on a sign-in or quota, it
continues the same thread once the account works again.

**Floating islands and the viewer.** Pages in the viewer pinch-zoom, can ask for their size, keep the size you gave
them, and support copy, select-all and Search With Google. Finished job islands leave a minute after their result;
report islands stay. A setting pins the islands to one display.

**Controlling apps.** A new, off-by-default Control Apps switch lets the agent press buttons and type in other apps
through Accessibility. You turn it on in the menu; the agent can turn it on only when you ask for it in words.

**Crewmates.** The agent can keep permanent workers, one per ongoing project, each a T3 thread it briefs, schedules and checks on for you.

**Also new:** a memory folder of small notes the agent keeps; a secret field in the composer whose text reaches the
agent only as a file path; goals that complete on your yes; a log line with its source for every event.

**Setup.** The Getting ready page keeps the "say the phrase" card in view. Setup now explains the Developer Tools
exemption that makes agents' new programs start faster (docs/setup.md).

**Under the hood.** About 33,000 lines of dead code removed in the 14 Nov cleanup, with roughly 14 bugs fixed, each
with a test. Over 4,000 automatic tests passed on this release's source.

**Docs.** The documentation was rewritten and consolidated; `docs/condensed-technical-explanation.md` is the short
version of how Heimdall is built.

Not in this release: the iPhone companion (planned for the next public build).

Install or upgrade: `brew upgrade --cask heimdall-alpha`, or download the zip from the release. The in-app updater
stays off in this alpha.

## 0.1.0-alpha.1 (5 Nov 2026)

First public alpha.
