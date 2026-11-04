"""Guard the current docs against superseded integration claims (point-in-time reports live in lab/history)."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class ReleaseDocumentation(unittest.TestCase):
    def test_extra_models_lock_ships_the_night_model(self):
        lock = (ROOT / 'scripts/extra-models.lock.json').read_text()
        self.assertIn('night/model.int8.onnx', lock)

    def test_smart_turn_shipping_migration_and_measured_flips(self):
        settings = (ROOT / 'docs/settings.md').read_text()
        self.assertIn('turn_backend_migrated: true', settings)
        self.assertIn('7 flips in 8,338', settings)
        self.assertIn('0.006', settings)
        self.assertIn('exact parity', settings)
        self.assertNotIn('already stores `"turn_backend": "cpu"` keeps CPU until changed', settings)

if __name__ == '__main__':
    unittest.main()
