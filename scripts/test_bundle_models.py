"""scripts/bundle_models.py: staged models are clones that later source changes never reach, and the Drama engine's
freshness follows content, not mtimes (BLD-22: a checkout that only touched mtimes stopped the personal bundle)."""
import os, sys, tempfile, time, unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bundle_models as b  # noqa: E402


def write(path, text, age=0):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    when = time.time() - age
    os.utime(path, (when, when))


class Place(unittest.TestCase):
    def test_a_staged_copy_keeps_its_bytes_when_the_source_changes(self):
        root = Path(tempfile.mkdtemp())
        write(root / "src.bin", "weights v1")
        b.place(root / "src.bin", root / "staged.bin")
        write(root / "src.bin", "weights v2")
        self.assertEqual((root / "staged.bin").read_text(), "weights v1")

    def test_an_existing_target_is_replaced(self):
        root = Path(tempfile.mkdtemp())
        write(root / "src.bin", "new")
        write(root / "staged.bin", "old")
        b.place(root / "src.bin", root / "staged.bin")
        self.assertEqual((root / "staged.bin").read_text(), "new")


class EngineFreshness(unittest.TestCase):
    def setUp(self):
        self.engine = Path(tempfile.mkdtemp())
        for name in ["Cargo.toml", "Cargo.lock", "src/main.rs", "vendor/mlx/lib.c"]:
            write(self.engine / name, f"source {name}", age=100)
        write(self.engine / "vendor/mlx/target/build.o", "build output", age=0)   # never a source
        self.binary = self.engine / "target/release/heimdall-drama3-engine"
        write(self.binary, "engine built from the sources", age=50)

    def current(self):
        return b.engine_is_current(self.binary, self.engine)

    def test_a_binary_newer_than_its_sources_is_current(self):
        self.assertTrue(self.current())

    def test_a_checkout_that_only_touches_mtimes_keeps_it_current(self):
        self.assertTrue(self.current())   # stamped while it passes the mtime rule
        for name in ["Cargo.toml", "src/main.rs"]:
            os.utime(self.engine / name)   # git rewrote them with the same bytes
        self.assertTrue(self.current())

    def test_a_changed_source_makes_it_stale(self):
        self.assertTrue(self.current())
        write(self.engine / "src/main.rs", "source changed")
        self.assertFalse(self.current())

    def test_an_older_binary_without_a_stamp_is_stale(self):
        os.utime(self.engine / "src/main.rs")
        self.assertFalse(self.current())

    def test_a_rebuild_is_current_and_a_later_revert_is_not(self):
        self.assertTrue(self.current())
        write(self.engine / "src/main.rs", "source changed", age=10)
        write(self.binary, "engine built from the changed sources", age=5)
        self.assertTrue(self.current())   # rebuilt: newer than its sources, stamped again
        write(self.engine / "src/main.rs", "source src/main.rs")   # back to the first sources, binary not rebuilt
        self.assertFalse(self.current())


if __name__ == "__main__":
    unittest.main()
