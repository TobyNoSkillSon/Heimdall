"""scripts/affected_crates.py: the day loop's selection never leaves out a crate a change can reach."""
import sys, unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import affected_crates as a  # noqa: E402

BY_DIR, DEPS = a.packages()
READS, LAB = a.path_reads(BY_DIR)


def pick(*files):
    return a.affected(list(files), BY_DIR, DEPS, READS, LAB)


class Selection(unittest.TestCase):
    def test_a_crate_brings_every_crate_that_depends_on_it(self):
        picked = pick("crates/archive/src/lib.rs")
        for name in ["heimdall-archive", "capture", "screen", "heimdall-night", "voicebank", "app"]:
            self.assertIn(name, picked)
        self.assertNotIn("splash", picked)

    def test_dependents_are_transitive(self):
        # home -> archive -> capture -> screen -> app
        self.assertTrue({"heimdall-home", "heimdall-archive", "capture", "screen", "app", "splash"} <= pick("crates/home/src/lib.rs"))

    def test_a_crate_whose_files_another_reads_brings_the_reader(self):
        # app's tests read ../agent/tests/fixtures/mock-adapter.py; app also depends on agent.
        self.assertIn("app", pick("crates/agent/tests/fixtures/mock-adapter.py"))
        # archive's sources name ../speakers/ paths without depending on speakers.
        self.assertIn("heimdall-archive", pick("crates/speakers/src/lib.rs"))

    def test_a_leaf_change_stays_small(self):
        picked = pick("crates/app/src/main.rs")
        self.assertIn("app", picked)
        # agent's tests read ../app/ fixtures, so agent and its users (t3, ui) follow; nothing below them.
        self.assertTrue(picked.isdisjoint({"listen", "speak", "speakers", "decide", "splash", "heimdall-archive", "capture"}))

    def test_files_outside_crates_select_everything(self):
        for f in ["Cargo.lock", "Cargo.toml", ".config/nextest.toml", "docs/install.md", "skills/heimdall/models/SKILL.md",
                  "scripts/bundle.sh", "ARCHITECTURE.md", "models/manifest.json", "crates/drama-engine/src/main.rs"]:
            self.assertIsNone(pick(f), f)

    def test_lab_reaches_only_the_crates_naming_its_fixtures(self):
        self.assertEqual(pick("lab/some-experiment/run.log"), set())
        self.assertIn("speak", pick("lab/phonon/voice.json"))
        self.assertIn("app", pick("lab/goals/mock-runner.py"))
        self.assertIn("ui", pick("lab/ui-renders/personalise/test-mark.svg"))

    def test_a_tasks_brief_selects_nothing(self):
        self.assertEqual(pick("BRIEF.md", "COORD.md"), set())

    def test_a_moved_file_counts_its_old_place_too(self):
        # git reports a rename under its new name only; a fixture moved out of a crate must still select that crate.
        import subprocess, tempfile
        repo = Path(tempfile.mkdtemp())
        run = lambda *c: subprocess.run(["git", "-C", str(repo), *c], check=True, capture_output=True)
        run("init", "-q", "-b", "main"); run("config", "user.email", "t@t"); run("config", "user.name", "t")
        (repo / "crates/a").mkdir(parents=True); (repo / "crates/a/fixture.txt").write_text("x" * 200)
        run("add", "-A"); run("commit", "-qm", "base")
        (repo / "crates/b").mkdir(parents=True); run("mv", "crates/a/fixture.txt", "crates/b/fixture.txt")
        root, a.ROOT = a.ROOT, repo
        try:
            self.assertIn("crates/a/fixture.txt", a.changed_files("main"))
        finally:
            a.ROOT = root


if __name__ == "__main__":
    unittest.main()
