import type { TranscriptEntry } from "./client/bridge.js";

/** One stable color per roster slot; readable on light and dark surfaces. */
export const AGENT_COLORS = ["#2f7de1", "#d9534f", "#2e9e6b", "#9b59d0"] as const;

export const agentColor = (index: number | undefined): string | undefined =>
  index === undefined ? undefined : AGENT_COLORS[index % AGENT_COLORS.length];

/** Transcript row label. Names appear only when the roster has more than one agent. */
export function entryLabel(entry: TranscriptEntry, multi: boolean): string {
  if (entry.role === "note") return "Chat";
  if (entry.to && entry.speaker) return `${entry.speaker} → ${entry.to}`;
  if (entry.role === "user") return multi && entry.speaker ? `You → ${entry.speaker}` : "You";
  return multi && entry.speaker ? entry.speaker : "Supervisor";
}

/** Second status line while agents talk to each other, or why they stopped. */
export function chainLine(state: {
  readonly transcript: readonly TranscriptEntry[];
  readonly status: string;
}): string {
  const last = state.transcript.at(-1);
  if (last?.role === "note") return last.text;
  if (last?.to && last.speaker && (state.status === "thinking" || state.status === "speaking"))
    return `${last.speaker} → ${last.to}`;
  return "";
}

/** " · Ada" while an agent is being waited on or heard; empty for one agent. */
export function speakerSuffix(state: {
  readonly participants: readonly unknown[];
  readonly speaker: string | null;
}): string {
  return state.participants.length > 1 && state.speaker ? ` · ${state.speaker}` : "";
}
