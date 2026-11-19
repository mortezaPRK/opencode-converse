/** The single stdio contract. Both ends must use exactly this protocol version. */
export const PROTOCOL_VERSION = 3 as const;

export type VoiceStatus =
  | "off"
  | "loading"
  | "listening"
  | "hearing"
  | "thinking"
  | "speaking"
  | "muted";

export type { Participant } from "./config.js";
import type { AgentChat, Participant } from "./config.js";

/** Local host preferences sent over stdio. Server credentials never appear here. */
export interface LocalSettings {
  /** The conversation's fixed roster; absent means one agent speaking the default voice. */
  participants?: readonly Participant[];
  inputDevice?: string;
  outputDevice?: string;
  asrThreads?: number;
  ttsThreads?: number;
  endpointing?: Endpointing;
  acknowledgement?: Acknowledgement;
  echoCancellation?: EchoCancellation;
  /** Whether an agent's reply that addresses another agent is relayed to that agent. */
  agentChat?: AgentChat;
}

/** Why the engine refused an agent-to-agent hand-off. */
export type ChainEndReason = "turns" | "time" | "ping_pong";

/** How long the engine waits in silence before treating the user's turn as finished. */
export type Endpointing = "responsive" | "patient";

/** Whether the engine speaks a short neutral cue while the supervisor is still thinking. */
export type Acknowledgement = "auto" | "off";

/** Whether the engine subtracts its own playback from the microphone signal. */
export type EchoCancellation = "on" | "off";

export type EngineMessage =
  | { type: "hello"; protocol: 3 }
  | {
      type: "status";
      state: Exclude<VoiceStatus, "off">;
      /** Participant the state is about: the addressee while thinking, the voice while speaking. */
      speaker?: string;
    }
  | {
      type: "prompt";
      turn: number;
      /** Participant the user addressed; the first participant when no name was spoken. */
      speaker: string;
      addressed: boolean;
      /** The transcript with the spoken name removed. */
      text: string;
      /** Set when another agent's reply addressed this participant: the relaying speaker. */
      from?: string;
    }
  | {
      type: "chain_end";
      turn: number;
      /** Why an agent-to-agent hand-off was refused: turn budget, wall clock, or ping-pong. */
      reason: ChainEndReason;
    }
  | {
      type: "interrupt";
      turn: number;
      /** Participant that held the floor or was addressed when the user cut in. */
      speaker?: string;
      /** Reply audio the output device actually played before the cut-off. */
      played_ms?: number;
      /** Reply audio generated for playback; `played_ms <= generated_ms`. */
      generated_ms?: number;
      /** Spoken text heard before the cut-off (bounded). Transcript only, never agent context. */
      heard?: string;
    }
  | { type: "error"; code: string; message: string; fatal: boolean };

export type HostMessage =
  | { type: "init"; protocol: 3; settings: LocalSettings }
  | { type: "reply_delta"; turn: number; speaker: string; reply: string; text: string }
  | { type: "reply_end"; turn: number; speaker: string; reply: string; text: string }
  | { type: "announcement"; speaker: string; reply: string; text: string }
  | {
      type: "turn_end";
      turn: number;
      reason: "completed" | "interrupted" | "disconnected" | "failed";
    }
  | { type: "prompt_failed"; turn: number; message: string }
  | { type: "mute"; on: boolean }
  | { type: "pause"; on: boolean }
  | { type: "shutdown" };

export const MAX_FRAME_BYTES = 1_048_576;
