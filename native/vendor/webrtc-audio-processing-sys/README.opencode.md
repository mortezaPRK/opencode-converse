# Local patch of webrtc-audio-processing-sys 2.1.0

Unmodified crates.io 2.1.0 (tonarino/webrtc-audio-processing, BSD-3-Clause, see
`COPYING` and the bundled `webrtc-audio-processing/` notices) except for `build.rs`, which gains
MSVC support. Changes are marked `opencode-converse` and adapted from upstream PR #102
(https://github.com/tonarino/webrtc-audio-processing/pull/102), which is not merged or released:

- pass `-Dcpp_std=c++20 -Db_vscrt=md` to meson on MSVC (designated initializers; release DLL CRT
  to match ONNX Runtime's prebuilt libraries);
- skip `v2_` symbol prefixing on MSVC (not reliable for COFF);
- compile the wrapper with `/std:c++20` instead of GCC-style flags.

Remove this directory and the `[patch.crates-io]` entry once an upstream release includes MSVC support.
