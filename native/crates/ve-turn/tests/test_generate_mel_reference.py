"""Development-only generator checks: stdlib Python, no NumPy/models/network.

Run explicitly with:
  python3 native/crates/ve-turn/tests/test_generate_mel_reference.py
Not a runtime, engine-build, or default CI dependency.
"""

import os
from pathlib import Path
import subprocess
import sys
import unittest


BOOTSTRAP = r"""
import builtins
import io
import runpy
import sys
import types
import urllib.request

numpy = types.ModuleType("numpy")
numpy.__version__ = sys.argv[2]
sys.modules["numpy"] = numpy
def mocked_fetch(*args, **kwargs):
    if numpy.__version__ != "2.1.3":
        raise RuntimeError("SOURCE_FETCHED_WITH_WRONG_NUMPY")
    return io.BytesIO(b"# mocked, untrusted source\n")
urllib.request.urlopen = mocked_fetch

# Never compile or execute even the mocked downloaded text. The original
# optimized generator reaches this guard; a hash mismatch must stop earlier.
original_compile = builtins.compile
def guarded_compile(source, filename, *args, **kwargs):
    if filename == "pinned audio_utils.py":
        raise RuntimeError("UNVERIFIED_SOURCE_REACHED_COMPILE")
    return original_compile(source, filename, *args, **kwargs)
builtins.compile = guarded_compile
runpy.run_path(sys.argv[1], run_name="__main__")
"""


class GeneratorHashPinning(unittest.TestCase):
    def assert_rejected_before_fetch_or_compilation(self, version, expected_error):
        generator = Path(__file__).with_name("generate_mel_reference.py")
        for flags, optimize in [([], None), (["-O"], None), ([], "1")]:
            with self.subTest(flags=flags, PYTHONOPTIMIZE=optimize):
                env = dict(os.environ)
                env.pop("PYTHONOPTIMIZE", None)
                if optimize is not None:
                    env["PYTHONOPTIMIZE"] = optimize
                result = subprocess.run(
                    [sys.executable, *flags, "-c", BOOTSTRAP, str(generator), version],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected_error, result.stderr)
                self.assertNotIn("UNVERIFIED_SOURCE_REACHED_COMPILE", result.stderr)
                self.assertNotIn("SOURCE_FETCHED_WITH_WRONG_NUMPY", result.stderr)

    def test_digest_mismatch_stops_before_compilation_in_all_optimization_modes(self):
        self.assert_rejected_before_fetch_or_compilation("2.1.3", "SHA-256 mismatch")

    def test_numpy_version_guard_survives_all_optimization_modes(self):
        self.assert_rejected_before_fetch_or_compilation("0.0", "requires NumPy 2.1.3")


if __name__ == "__main__":
    unittest.main()
