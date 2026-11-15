"""Explicit, model-free fixture regeneration; needs numpy==2.1.3 and network.

Run from any directory. Rust tests consume the committed outputs offline.
The pinned upstream methods are loaded unchanged, not reimplemented here.
"""

import ast
import hashlib
from pathlib import Path
from types import SimpleNamespace
from typing import List
from urllib.request import urlopen

import numpy as np

if np.__version__ != "2.1.3":
    raise SystemExit("Fixture regeneration requires NumPy 2.1.3")
BASE = "https://raw.githubusercontent.com/huggingface/transformers/v4.46.3/"
SOURCES = {
    "src/transformers/audio_utils.py": "e4b5ee29dc3c56363f81096af3a76f112ecdaac0f9c1be9aa3125fc3913321df",
    "src/transformers/models/whisper/feature_extraction_whisper.py": "92a7d57aa3791e834e8efc12455ae49f044c6c19a1152d8521ffed08e139e2d8",
}
sources = {}
for path, digest in SOURCES.items():
    with urlopen(BASE + path, timeout=30) as response:
        source = response.read()
    if hashlib.sha256(source).hexdigest() != digest:
        raise SystemExit(f"SHA-256 mismatch for pinned reference source: {path}")
    sources[path] = source.decode()

audio = {}
exec(compile(sources["src/transformers/audio_utils.py"], "pinned audio_utils.py", "exec"), audio)
whisper = ast.parse(sources["src/transformers/models/whisper/feature_extraction_whisper.py"])
cls = next(node for node in whisper.body if isinstance(node, ast.ClassDef) and node.name == "WhisperFeatureExtractor")
methods = [node for node in cls.body if isinstance(node, ast.FunctionDef) and node.name in
           ["zero_mean_unit_var_norm", "_np_extract_fbank_features"]]
for method in methods:
    method.decorator_list = []  # Invoke the static normalization function directly.
scope = {"np": np, "List": List, **{name: audio[name] for name in ["spectrogram", "window_function"]}}
exec(compile(ast.Module(body=methods, type_ignores=[]), "pinned WhisperFeatureExtractor methods", "exec"), scope)
filters = audio["mel_filter_bank"](201, 80, 0.0, 8000.0, 16000, norm="slaney", mel_scale="slaney")
extractor = SimpleNamespace(n_fft=400, hop_length=160, mel_filters=filters)

# Integer arithmetic yields the exact same f32 waveform in Python and Rust.
# Includes DC, nonperiodic broadband detail, and a changing envelope.
def waveform(length):
    return np.array([(((i * 73 + i * i * 19) % 65521) - 32760) / 65536.0
                     * (0.25 + (i % 997) / 1024.0) + 0.125 for i in range(length)], dtype=np.float32)

data = Path(__file__).parent / "data"
inputs = {
    "nonzero": waveform(128000),
    "silence": np.zeros(128000, dtype=np.float32),
    "left_padded": np.concatenate([np.zeros(128000 - 1237, dtype=np.float32), waveform(1237)]),
}
for name, chunk in inputs.items():
    # The runtime has already left-padded before normalization: the whole window
    # is valid input, so no attention-mask trimming is applied in the reference.
    normalized = scope["zero_mean_unit_var_norm"]([chunk], None)[0]
    features = scope["_np_extract_fbank_features"](extractor, normalized[None, :], "cpu")[0]
    assert features.shape == (80, 800)
    raw = np.asarray(features, dtype="<f4").tobytes()
    path = data / f"logmel_{name}.f32"
    path.write_bytes(raw)
    print(path.name, hashlib.sha256(raw).hexdigest())
assert np.array_equal(filters.astype("<f4"), np.fromfile(data / "mel_filters.f32", dtype="<f4").reshape(201, 80))
