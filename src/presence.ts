import type { IconName } from "@opencode/plugin/gui";
import type { VoiceStatus } from "./protocol.js";

/**
 * What the conversation looks like at a glance. The Rust engine emits a small,
 * fixed set of voice states; this module is the one place that turns them into
 * the words, artwork, colour and motion each host renders. Terminal and Desktop
 * surfaces read it, so a state never means two different things.
 */
export type PresenceState = VoiceStatus | "error";

/** The colour family a surface uses; each host maps it to its own palette. */
export type PresenceTone = "neutral" | "muted" | "success" | "info" | "warning" | "danger";

export interface Presence {
  readonly state: PresenceState;
  /** The state's short name, such as "Hearing you". */
  readonly label: string;
  /** One sentence that says what is happening and what the user can do. */
  readonly detail: string;
  /** Artwork from the Desktop icon catalog. */
  readonly icon: IconName;
  /** The colour family of the icon and the terminal chip. */
  readonly tone: PresenceTone;
  /** Terminal artwork, one entry per frame; a single frame never animates. */
  readonly frames: readonly string[];
  /** Milliseconds between frames; used only when there is more than one. */
  readonly interval: number;
  /** A conversation is loaded, so the engine is starting, listening, or answering. */
  readonly active: boolean;
}

/** How long an error stays on the status surface before the engine state shows again. */
export const PRESENCE_ERROR_MS = 12_000;

const STATES: Record<VoiceStatus, Omit<Presence, "state" | "active">> = {
  off: {
    label: "Idle",
    detail: "Not running. Start a conversation to talk.",
    icon: "waveform",
    tone: "muted",
    frames: ["○"],
    interval: 0,
  },
  loading: {
    label: "Starting",
    detail: "Waking the local voice engine.",
    icon: "waveform",
    tone: "info",
    frames: ["◐", "◓", "◑", "◒"],
    interval: 120,
  },
  listening: {
    label: "Listening",
    detail: "Mic open. Say what you need.",
    icon: "outline-wave-audio",
    tone: "success",
    frames: ["◉", "◎", "○", "◎"],
    interval: 480,
  },
  hearing: {
    label: "Hearing you",
    detail: "Capturing your request. Pause to send it.",
    icon: "outline-audio",
    tone: "success",
    frames: ["▁▃▅", "▂▅▃", "▃▂▅", "▅▃▁"],
    interval: 160,
  },
  thinking: {
    label: "Thinking",
    detail: "Working on your request.",
    icon: "outline-brain",
    tone: "info",
    frames: ["◐", "◓", "◑", "◒"],
    interval: 160,
  },
  speaking: {
    label: "Speaking",
    detail: "Reading the reply aloud.",
    icon: "speech-bubble",
    tone: "info",
    frames: ["▅▃▁", "▃▅▃", "▁▃▅", "▃▁▅"],
    interval: 180,
  },
  muted: {
    label: "Muted",
    detail: "Mic off. The conversation keeps running.",
    icon: "outline-eye-slash",
    tone: "warning",
    frames: ["⊘"],
    interval: 0,
  },
};

const VOICE_STATUSES: readonly VoiceStatus[] = [
  "off",
  "loading",
  "listening",
  "hearing",
  "thinking",
  "speaking",
  "muted",
];

/**
 * Narrows a status that crossed an IPC boundary in a wider schema. An absent or
 * unknown value reads as stopped rather than inventing a state the engine never
 * reported.
 */
export function normalizeVoiceStatus(value: string | undefined): VoiceStatus {
  return value && (VOICE_STATUSES as readonly string[]).includes(value)
    ? (value as VoiceStatus)
    : "off";
}

/**
 * The presentation of one engine state, or of a recent error while the engine
 * state stays what it is. An error replaces the label and detail but not
 * `active`, so a running conversation keeps its controls.
 */
export function presence(state: VoiceStatus, error?: string): Presence {
  if (error)
    return {
      state: "error",
      label: "Error",
      detail: error,
      icon: "circle-exclamation",
      tone: "danger",
      frames: ["⚠"],
      interval: 0,
      active: state !== "off",
    };
  return { state, ...STATES[state], active: state !== "off" };
}

/** The frame a terminal chip shows at `frame`, wrapping around the loop. */
export function presenceGlyph(value: Presence, frame: number): string {
  const count = value.frames.length;
  return value.frames[((frame % count) + count) % count] ?? "";
}
