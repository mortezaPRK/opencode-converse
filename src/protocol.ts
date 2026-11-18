/** The single stdio contract. Both ends must use exactly this protocol version. */
export const PROTOCOL_VERSION = 1 as const;

export type VoiceStatus =
  | "off"
  | "loading"
  | "listening"
  | "hearing"
  | "thinking"
  | "speaking"
  | "muted";

/** Local host preferences sent over stdio. Server credentials never appear here. */
export interface LocalSettings {
  inputDevice?: string;
  outputDevice?: string;
  asrThreads?: number;
  ttsThreads?: number;
  useGpu?: boolean;
}

export type EngineMessage =
  | { type: "hello"; protocol: 1 }
  | { type: "status"; state: Exclude<VoiceStatus, "off"> }
  | { type: "prompt"; turn: number; text: string }
  | { type: "interrupt"; turn: number }
  | { type: "error"; code: string; message: string; fatal: boolean };

export type HostMessage =
  | { type: "init"; protocol: 1; settings: LocalSettings }
  | { type: "reply_delta"; turn: number; reply: string; text: string }
  | { type: "reply_end"; turn: number; reply: string; text: string }
  | { type: "announcement"; reply: string; text: string }
  | {
      type: "turn_end";
      turn: number;
      reason: "completed" | "interrupted" | "disconnected" | "failed";
    }
  | { type: "prompt_failed"; turn: number; message: string }
  | { type: "mute"; on: boolean }
  | { type: "shutdown" };

export const MAX_FRAME_BYTES = 1_048_576;
