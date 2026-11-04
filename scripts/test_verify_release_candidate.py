"""Fast checks of verify_release_candidate.py's bundle-content rules (no signing, no candidate needed)."""
import importlib.util
import plistlib
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location('verify_release_candidate', Path(__file__).with_name('verify_release_candidate.py'))
verify = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify)
MACHO = b'\xcf\xfa\xed\xfe' + b'\0' * 60

def bundle_with_islands(root, identifier=verify.ISLANDS_HELPER_ID, cli=True, helper=True):
    bundle = Path(root)/'Heimdall.app'
    (bundle/'Contents/MacOS').mkdir(parents=True)
    if cli:
        (bundle/verify.ISLANDS_CLI).write_bytes(MACHO)
    app = bundle/verify.ISLANDS_HELPER
    (app/'Contents/MacOS').mkdir(parents=True)
    if helper:
        (app/'Contents/MacOS/FloatingIslands').write_bytes(MACHO)
    (app/'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier': identifier, 'CFBundleExecutable': 'FloatingIslands', 'LSUIElement': True}))
    return bundle

class IslandsContent(unittest.TestCase):
    def test_the_helper_and_cli_are_checked_with_their_own_identifiers(self):
        with tempfile.TemporaryDirectory() as d:
            bundle = bundle_with_islands(d)
            self.assertEqual(verify.islands_code(bundle), [(bundle/'Contents/MacOS/islands', 'dev.heimdall.app'),
                                                           (bundle/'Contents/Helpers/FloatingIslands.app', 'dev.heimdall.floatingislands')])

    def test_a_missing_cli_or_helper_or_a_wrong_identifier_fails(self):
        for kwargs, message in [({'cli': False}, 'islands CLI'), ({'helper': False}, 'helper missing'), ({'identifier': 'dev.vault.floatingislands'}, 'identifier')]:
            with tempfile.TemporaryDirectory() as d, self.subTest(kwargs):
                with self.assertRaisesRegex(AssertionError, message):
                    verify.islands_code(bundle_with_islands(d, **kwargs))

    def test_the_bundle_script_builds_the_helper_and_signs_it_before_the_outer_bundle(self):
        script = (Path(__file__).with_name('bundle.sh')).read_text()
        call = script.index('scripts/bundle_islands.sh "$BUNDLE" "$IDENTITY"')
        self.assertLess(call, script.index('codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$BUNDLE"\n'))
        helper = (Path(__file__).with_name('bundle_islands.sh')).read_text()
        self.assertIn('nice -n 19 swift build -c release -j 2', helper)
        self.assertIn('Contents/Helpers/FloatingIslands.app', helper)
        self.assertIn(verify.ISLANDS_HELPER_ID, helper)

if __name__ == '__main__':
    unittest.main()
