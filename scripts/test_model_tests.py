"""scripts/model_tests.py: every `#[ignore = "model: ..."]` test reaches the full check's model step, and nothing else."""
import sys, tempfile, unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import model_tests as m  # noqa: E402

SOURCE = '''
#[test]
#[ignore = "model: loads Decider V6 16-bit (models/decide/d1)"]
fn decider_answers() {}

#[ignore = "model: Parakeet"] #[test]
async fn listens_to_a_file() {}

#[test]
#[ignore = "needs HEIMDALL_LAB_DATA"]
fn a_lab_probe() {}

#[test]
#[ignore]
fn a_helper() {}

#[test]
fn a_default_test() {}

    #[test]
    #[ignore="model: nested in a module"]
    fn inner_model_test() -> anyhow::Result<()> { Ok(()) }
'''


class Selection(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        (self.root / "src").mkdir()
        (self.root / "src/lib.rs").write_text(SOURCE)
        (self.root / "target/debug").mkdir(parents=True)
        (self.root / "target/debug/stale.rs").write_text('#[ignore = "model: build output"]\nfn stale() {}\n')

    def test_only_tests_marked_model_are_selected(self):
        self.assertEqual(m.marked(self.root), {"decider_answers", "listens_to_a_file", "inner_model_test"})

    def test_the_filterset_names_package_and_test(self):
        self.assertEqual(m.filterset({"decide": {"b", "a"}, "app": {"c"}}),
                         "(package(app) & test(/(^|::)c$/)) | (package(decide) & test(/(^|::)a$/)) | "
                         "(package(decide) & test(/(^|::)b$/))")

    def test_no_marks_give_an_empty_filterset(self):
        self.assertEqual(m.filterset({}), "")

    def test_the_workspace_scan_runs(self):
        # The real tree: every package scans, and Splash is kept apart from the rest.
        packages = m.packages()
        self.assertIn("app", packages)
        self.assertIn("splash", packages)


if __name__ == "__main__":
    unittest.main()
