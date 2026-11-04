"""scripts/check.sh: a second run in the same worktree stops at once and names the running one; --status reports it."""
import os, subprocess, tempfile, time, unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRATCH = tempfile.mkdtemp()
LOCK = Path(SCRATCH) / "heimdall-check.lock"  # a private lock, so a real check running here is never touched
ENV = {**{k: v for k, v in os.environ.items() if k != "HEIMDALL_CHECK_LOCKED"}, "HEIMDALL_CHECK_LOCK": str(LOCK)}


class Lock(unittest.TestCase):
    def setUp(self):
        # Stand in for a running check: hold the lock as scripts/check.sh does, with its info line.
        self.holder = subprocess.Popen(["lockf", "-k", "-s", "-t", "0", str(LOCK), "sleep", "30"])
        # Wait until the stand-in really holds the lock, or the check under test would take it and run for real.
        for _ in range(300):
            if subprocess.run(["lockf", "-k", "-s", "-t", "0", str(LOCK), "true"]).returncode == 75:
                break
            time.sleep(0.05)
        else:
            self.fail("the stand-in never took the lock")
        LOCK.with_name(LOCK.name + ".info").write_text("pid 1, tests since 00:00:00, log /x/target/check/run\n")

    def tearDown(self):
        self.holder.kill()
        self.holder.wait()
        LOCK.with_name(LOCK.name + ".info").unlink(missing_ok=True)

    def test_a_second_run_stops_at_once_and_names_the_first(self):
        began = time.time()
        r = subprocess.run([str(ROOT / "scripts/check.sh"), "--change"], capture_output=True, text=True, env=ENV)
        self.assertEqual(r.returncode, 75, r.stderr)
        self.assertIn("/x/target/check/run", r.stderr)
        self.assertLess(time.time() - began, 5, "it must not wait for the running check")

    def test_status_reports_the_running_check(self):
        r = subprocess.run([str(ROOT / "scripts/check.sh"), "--status"], capture_output=True, text=True, env=ENV)
        self.assertEqual(r.returncode, 0)
        self.assertIn("running: pid 1", r.stdout)


if __name__ == "__main__":
    unittest.main()
