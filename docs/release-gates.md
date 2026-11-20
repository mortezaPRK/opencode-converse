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
- Engine and speech-model paths are fixed under the app state folder. No plugin
  option or environment variable can redirect them. This version's engine is downloaded
  once into the state folder and verified against `assets/engines.json`, and models are reused across engine versions; device
  preferences stay in host storage.
- Pause silences spoken output without cancelling the turn, discarding queued
  speech, or stopping the runtime; resume continues the held speech. Verify on a
  real device that nothing is audible while paused and that Stop still releases audio.
- Barge-in while paused discards the held speech and starts the user's turn.
- Bounded protocol frames and queues fail clearly rather than growing without limit.
- No default logs contain transcripts, credentials, or raw native-library output.

## Speech quality

- English ASR accuracy and spoken technical terms are evaluated on a fixed sample.
- Measure first-audio and interruption latency on representative hardware.
- Compare `endpointing: "responsive"` and `"patient"` on a fixed sample with natural
  mid-sentence pauses: record premature cut-offs and added end-of-turn latency.
- Acknowledgement: with a slow supervisor, the neutral cue starts about 0.6 s after
  the turn is sent (cached, no synthesis delay), is spoken at most once per turn,
  never claims success, and is absent from the agent context and transcript. Check
  that it is not spoken for empty/rejected transcripts, after barge-in, while paused
  or muted, or for worker announcements; that a reply starting during the cue
  follows it without overlap or a dropped first word; that barge-in cuts it
  instantly; and that `acknowledgement: "off"` is silent.
- First-audio latency: record reply-first-delta to first audible sound on
  representative hardware (run `speech_quality`'s timeline); the early first-chunk
  cut must not produce audibly choppy prosody on a fixed set of replies with commas.
- Check the Interrupted note in the transcript against what was actually audible on
  a real device: the heard text must match what was played, within a word or two,
  and must never be added to agent context.
- **Echo/self-trigger gate (still open).** Echo cancellation (`echoCancellation: "on"`,
  WebRTC AEC3) has only been measured offline against simulated echo
  (`aec_quality`). On real laptop speakers, at several volumes and rooms, and with
  Bluetooth and USB devices: count false interruptions of a long reply (must be zero
  over at least 10 minutes of assistant speech), confirm you can interrupt with a
  normal voice within about 0.3 s, check that the first words of the interruption are
  transcribed, and that a quiet user over loud speech is not clipped by the
  canceller. Until this passes, document headphones as the safe choice and keep
  `echoCancellation: "off"` available. Also test with a different output and input
  device clock (USB mic, Bluetooth speaker) for drift.
- **Multi-agent voices.** With `agents` configured on real hardware: the four voices
  (`zoey`, `maeve`, `easton`, `preston`) are clearly distinguishable from each other
  and from the default voice; addressing by leading and trailing name works with real
  ASR output (try names a recogniser tends to mishear) and an unnamed prompt goes to
  the first agent; the acknowledgement cue plays in the addressee's voice; speaking
  over any agent cuts it at once and drops queued speech; each agent's replies reach
  only its own session and its own followed sessions; transcripts and the sidebar
  show the right name and colour. Confirm voice switching adds no audible gap
  (measured offline: first audio about 20-90 ms per voice on Apple silicon).
- **Agent-to-agent cost and runaway gate.** With `agentChat: "on"` and 2 to 4 agents:
  prompt an agent to hand off repeatedly ("keep asking Grace and Ada to hand this
  back and forth") and confirm the chain stops by itself at 4 agent turns (and never
  beyond 2 round trips for one pair), shows the "Agent chat ended" note, and returns
  to listening; count model calls in the server log for one such turn (must be at most
  4 supervisor turns). Confirm that speaking mid-chain cancels the active voice and
  the queued ones, ends the chain, and starts no further model turn; that muting does
  too; that a worker announcement never starts a chain; and that `agentChat: "off"`
  makes no hand-offs. Check the 90-second cap with a slow model. Record the token
  cost of a typical four-turn chain before shipping `"on"` as the default.
- **Prompter lock gate.** With two agents: Ada follows a session; Grace trying
  `send_message`, `interrupt_session` or `attach_session` on it is refused with Ada's
  name; saying "Grace, take over that session" moves it (Grace can prompt, Ada can no
  longer); an agent asking the other to take it over (relayed line) is refused;
  announcements for the session are spoken in the holder's voice.
- Test default-device changes, Bluetooth devices, and microphone removal.

## Distribution and compliance

- Pin model versions, sizes and hashes; verify downloads before activation.
- Review exact model cards, datasets/voice terms, native-library notices, and
  transitive assets. Verify the Phonon Apache-2.0 notice and Kyutai (Pocket TTS /
  Mimi) attribution for the actual bundle.
- Phonon voices (`gradium/phonon-voices`, pinned commit `0ee17205f254b5abc43b266c8a0f215565c352f7`):
  the repository declares `license: apache-2.0` in its card metadata only and ships no
  `LICENSE` file; the Apache-2.0 text installed beside the voices is the one from
  `gradium/phonon-31m-en`. Confirm with Gradium (or from the dataset terms) that the
  four shipped embeddings (Zoey, Maeve, Easton, Preston) carry no extra likeness or
  attribution terms before redistributing, and keep the Gradium attribution.
- Produce a third-party inventory, notices, and any required source materials.
- Review npm and Cargo dependency advisories. Do not use forced dependency upgrades
  as a substitute for testing the supported SDK.
- Attach each target's engine to the version's GitHub release as
  `opencode-converse-<version>-<platform>-<arch>[.exe]` and pin its size and
  SHA-256 in `assets/engines.json` before packing. Verify a clean machine downloads,
  verifies, and starts the engine without a compiler or Node build step, and that
  `npm pack` ships no engine binary.
- Test the packed npm artifact and native bundle, not just a workspace checkout.
- Public repository name/ownership, signing identities, and publication are
  separate explicit actions. No workflow in this repository publishes releases.
