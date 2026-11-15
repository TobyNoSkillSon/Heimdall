"""scripts/dialog_watch.py: recorded window text in, the right requester, verdict and button out; never Allow."""
import sys, tempfile, unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dialog_watch as d  # noqa: E402

HOME = "/Users/t"
# 13 Nov 22:45, as SecurityAgent showed it (the bare binary's read of the T3 pairing).
NIGHT = d.Window("SecurityAgent", "", [
    "heimdall wants to use your confidential information stored in “dev.heimdall.t3” in your keychain.",
    "To allow this, enter the “login” keychain password.",
    "Password:",
], ["Always Allow", "Deny", "Allow"])
BARE = (4242, "/Users/t/Work/Projects/Heimdall/target/release/heimdall")
WORKTREE = (4243, "/Users/t/Work/Projects/Heimdall/.claude/worktrees/keychain-safe/target/test/debug/heimdall")
INSTALLED = (500, "/Users/t/Applications/Heimdall.app/Contents/MacOS/Heimdall")
OTHER = (600, "/usr/local/bin/heimdall")


class Parsing(unittest.TestCase):
    def test_the_13_nov_keychain_dialog(self):
        self.assertEqual(d.requester(NIGHT), ("heimdall", "dev.heimdall.t3"))

    def test_other_recorded_dialogs(self):
        cases = [
            (d.Window("SecurityAgent", "", ["“Heimdall” wants to access key “Heimdall API provider: Anthropic” in your keychain."]), ("Heimdall", "Heimdall API provider: Anthropic")),
            (d.Window("SecurityAgent", "", ["security wants to use the “login” keychain."]), ("security", "login")),
            (d.Window("SecurityAgent", "", ["cargo wants to make changes."], ["Cancel", "OK"]), ("cargo", None)),
            (d.Window("UserNotificationCenter", "", ["“Terminal” would like to access files in your Documents folder."], ["Don’t Allow", "Allow"]), ("Terminal", None)),
            (d.Window("CoreServicesUIAgent", "", ["“friends-peer” can’t be opened because Apple cannot check it for malicious software."], ["Move to Trash", "Done"]), ("friends-peer", None)),
            (d.Window("UserNotificationCenter", "", ["Do you want the application “heimdall” to accept incoming network connections?"], ["Deny", "Allow"]), ("heimdall", None)),
            (d.Window("SecurityAgent", "", ["Something unrelated"]), (None, None)),
        ]
        for w, want in cases:
            self.assertEqual(d.requester(w), want, w.texts)


class Verdict(unittest.TestCase):
    def test_a_bare_or_worktree_binary_is_ours(self):
        self.assertEqual(d.classify("heimdall", [BARE], HOME), (True, BARE[1]))
        self.assertTrue(d.classify("heimdall", [BARE, WORKTREE], HOME)[0])
        self.assertTrue(d.classify("python3", [(7, "/private/tmp/claude-501/x/venv/bin/python3")], HOME)[0])

    def test_the_installed_app_or_any_other_app_is_never_ours(self):
        self.assertFalse(d.classify("Heimdall", [INSTALLED, BARE], HOME)[0])
        self.assertFalse(d.classify("Heimdall", [(1, "/Applications/Heimdall.app/Contents/MacOS/Heimdall")], HOME)[0])
        # Same name, one copy outside our folders: not ours.
        self.assertFalse(d.classify("heimdall", [BARE, OTHER], HOME)[0])
        self.assertFalse(d.classify("Terminal", [(2, "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal")], HOME)[0])
        self.assertEqual(d.classify("heimdall", [], HOME), (False, "no running process named heimdall"))
        self.assertFalse(d.classify(None, [BARE], HOME)[0])
        # Review 14 Nov: a downloaded app run through App Translocation, or another project's dev build, is not ours.
        self.assertFalse(d.classify("Foo", [(8, "/private/var/folders/ab/T/AppTranslocation/X/d/Foo.app/Contents/MacOS/Foo")], HOME)[0])
        self.assertFalse(d.classify("tool", [(9, "/Users/t/Code/other/target/debug/tool")], HOME)[0])
        self.assertFalse(d.classify("tool", [(9, "/tmp/somebody/tool")], HOME)[0])

    def test_the_press_matches_the_window_by_its_text_not_its_index(self):
        # The press script compares JSON.stringify of the re-read window with this: same keys, order, no spaces,
        # curly quotes kept as they are.
        self.assertEqual(d.expected(NIGHT)[:60], '{"title":"","texts":["heimdall wants to use your confidentia')
        self.assertIn("“dev.heimdall.t3”", d.expected(NIGHT))
        self.assertTrue(d.expected(NIGHT).endswith('"buttons":["Always Allow","Deny","Allow"]}'))

    def test_only_refusing_buttons_are_ever_pressed(self):
        self.assertEqual(d.safe_button(["Always Allow", "Deny", "Allow"]), "Deny")
        self.assertEqual(d.safe_button(["Don’t Allow", "Allow"]), "Don’t Allow")
        self.assertEqual(d.safe_button(["Cancel", "OK"]), "Cancel")
        for b in (["Allow"], ["Always Allow", "OK"], ["Move to Trash", "Done"], ["Open"], []):
            self.assertIsNone(d.safe_button(b), b)


class Watch(unittest.TestCase):
    def run_watch(self, frames, procs, dismiss=True):
        pressed, frames = [], list(frames)
        status = Path(tempfile.mkdtemp()) / "status.md"
        clock = [0.0]

        def reader():
            f = frames.pop(0) if frames else []
            if isinstance(f, Exception):
                raise f
            return f

        def sleep(s):
            clock[0] += s
        n = len(frames)
        code = d.watch(status, 20, n * 20 / 3600 - 1e-9, dismiss, False, reader=reader, lister=lambda: procs,
                       presser=lambda w, b: pressed.append((w.process, b)), sleep=sleep, now=lambda: clock[0], home=HOME)
        return code, status.read_text() if status.exists() else "", pressed

    def test_our_dialog_is_denied_once_and_recorded_once(self):
        code, text, pressed = self.run_watch([[NIGHT], [NIGHT], [NIGHT], []], [BARE, INSTALLED])
        self.assertEqual(code, 0)
        self.assertEqual(pressed, [("SecurityAgent", "Deny")])
        lines = [l for l in text.splitlines() if "dialog ·" in l]
        self.assertEqual(len(lines), 1, text)
        self.assertIn("requester heimdall (ours: /Users/t/Work/Projects/Heimdall/target/release/heimdall)", lines[0])
        self.assertIn("item dev.heimdall.t3", lines[0])
        self.assertIn("pressed Deny", lines[0])

    def test_the_installed_heimdalls_dialog_is_only_noted(self):
        theirs = d.Window("SecurityAgent", "", ["Heimdall wants to use your confidential information stored in “dev.heimdall.t3” in your keychain."], ["Always Allow", "Deny", "Allow"])
        code, text, pressed = self.run_watch([[theirs]], [INSTALLED])
        self.assertEqual(pressed, [])
        self.assertIn("not ours: Heimdall runs from /Users/t/Applications/Heimdall.app", text)
        self.assertIn("left for the morning", text)

    def test_record_only_mode_never_presses(self):
        _, text, pressed = self.run_watch([[NIGHT]], [BARE], dismiss=False)
        self.assertEqual(pressed, [])
        self.assertIn("left for the morning", text)

    def test_a_dialog_that_comes_back_is_recorded_again(self):
        _, text, pressed = self.run_watch([[NIGHT], [], [NIGHT]], [BARE])
        self.assertEqual(len(pressed), 2)
        self.assertEqual(text.count("dialog · SecurityAgent"), 2)

    def test_a_watcher_that_cannot_read_says_so_once_and_when_it_can_again(self):
        err = RuntimeError("execution error: Not authorised to send Apple events to System Events. (-1743)")
        code, text, _ = self.run_watch([err, err, []], [])
        self.assertEqual(text.count("cannot read dialogs"), 1, text)
        self.assertIn("-1743", text)
        self.assertIn("reads dialogs again", text)

    def test_a_failed_press_is_recorded(self):
        def refuse(w, b):
            raise RuntimeError("AXPress refused")
        line = d.handle(NIGHT, [BARE], HOME, True, refuse)
        self.assertIn("pressing Deny failed (AXPress refused); left for the morning", line)


if __name__ == "__main__":
    unittest.main()
