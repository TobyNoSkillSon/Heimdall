"""No-model checks of ack_clips.sh: it refuses while Heimdall runs and states its cost before doing anything."""
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

SCRIPT = Path(__file__).with_name('ack_clips.sh')


def run(*args, pattern):
    env = {**os.environ, 'ACK_CLIPS_APP_PATTERN': pattern}
    return subprocess.run(['/bin/bash', str(SCRIPT), *args], env=env, capture_output=True, text=True, timeout=30)


class AckClips(unittest.TestCase):
    def test_refuses_while_heimdall_runs_and_touches_nothing(self):
        marker = f'FakeHeimdallApp{os.getpid()}'
        fake = subprocess.Popen(['/bin/bash', '-c', f'exec -a {marker} sleep 30'])
        try:
            time.sleep(0.2)
            with tempfile.TemporaryDirectory() as temp:
                lab = Path(temp) / 'lab'
                result = run('--lab', str(lab), '--into', temp, '--apply', pattern=marker)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn('Quit Heimdall first', result.stderr)
                self.assertFalse(lab.exists(), 'nothing created')
                self.assertNotIn('cost', result.stdout)
        finally:
            fake.kill()
            fake.wait()

    def test_plan_states_cost_and_stops_before_building_or_loading(self):
        with tempfile.TemporaryDirectory() as temp:
            lab = Path(temp) / 'lab'
            result = run('--lab', str(lab), '--into', temp, '--plan', pattern='NoSuchProcess-ack-clips-test')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('48 acks x 2 takes = 96', result.stdout)
            self.assertIn('29 GB RAM', result.stdout)
            self.assertIn('dry run', result.stdout)
            self.assertFalse(lab.exists())

    def test_requires_lab_and_destination(self):
        for args in ([], ['--lab', '/tmp/x'], ['--into', '/tmp/x'], ['--lab', 'a', '--into', 'b', '--takes', '0']):
            self.assertEqual(run(*args, pattern='NoSuchProcess-ack-clips-test').returncode, 2, args)


if __name__ == '__main__':
    unittest.main()
