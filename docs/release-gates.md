# Release gates

Do not publish the npm package or native bundles until these gates have evidence.
Unit tests and compilation alone are not end-to-end host verification.

## Host and platform matrix

Initial targets are macOS arm64, Linux x64, and Windows x64, with terminal and
Desktop clients connected to both local and remote OpenCode servers. Other
architectures are not implied by operating-system support.

- Clean source build and installed-package load using OpenCode 2.3.0.
- Signed native bundles with all required runtime libraries, without a compiler
  or a separate Node runtime on the user's machine.
- macOS microphone access, signing, quarantine, and notarization behavior.
- Windows DLL resolution, non-ASCII paths, microphone privacy, and process cleanup.
- Linux audio-server/device coverage and a documented libc/runtime baseline.
- Actual microphone → supervisor → audible reply on every target combination.

## Safety and lifecycle

- The selected server, workspace, and supervisor remain fixed throughout a run.
- A missing or mismatched remote plugin reports an error without local fallback.
- Disconnects do not resend user prompts or replay stale speech. Restore state
  from the same server after reconnecting; do not assume event delivery is durable.
- Stop, reload, window closure, and parent-process death release audio and the
  process lock. Startup cancellation works while models load.
- Mute closes capture without unloading speech models; verify the OS microphone
  indicator. Speaker playback remains independent.
- Late generation results and interrupted reply chunks cannot restart speech.
- Worker/project data cannot make the model approve a permission request.
- Remote settings cannot choose a local executable, model path, or output file.
- Bounded protocol frames and queues fail clearly rather than growing without limit.
- No default logs contain transcripts, credentials, or raw native-library output.

## Speech quality

- English ASR accuracy and spoken technical terms are evaluated on a fixed sample.
- Measure first-audio and interruption latency on representative hardware.
- Measure false prompts from the assistant's own voice on speakers. Require
  headphones unless speaker use has passed an explicit echo/self-trigger test.
- Test default-device changes, Bluetooth devices, and microphone removal.

## Distribution and compliance

- Pin model versions, sizes and hashes; verify downloads before activation.
- Review exact model cards, datasets/voice terms, native-library notices, and
  transitive assets. Address eSpeak NG obligations for the actual bundle.
- Produce a third-party inventory, notices, and any required source materials.
- Review npm and Cargo dependency advisories. Do not use forced dependency upgrades
  as a substitute for testing the supported SDK.
- Test the packed npm artifact and native bundle, not just a workspace checkout.
- Public repository name/ownership, signing identities, and publication are
  separate explicit actions. No workflow in this repository publishes releases.
