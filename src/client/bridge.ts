import {
  isInvalidRequestError,
  isConflictError,
  isSessionNotFoundError,
  isLocationNotFoundError,
  isUnauthorizedError,
  isForbiddenError,
  isModelNotFoundError,
  isAgentNotFoundError,
  type OpenCodeClient,
  type OpenCodeEvent,
} from "@opencode/client";
import { Converse, WORKER_UPDATE_MARKER } from "../rpc.js";
import type { RuntimeConfig } from "../config.js";
import type { EngineMessage, HostMessage, LocalSettings, VoiceStatus } from "../protocol.js";
import { startRuntime, type Runtime, type RuntimeOptions } from "./runtime.js";
import { waitForConverse } from "./readiness.js";

export interface Target {
  readonly serverID: string;
  readonly directory: string;
}
/** What was heard of a reply the user cut off. Display only; the agent never sees it. */
export interface Interruption {
  /** Spoken text heard before the cut-off. */
  heard?: string;
  /** Share of generated reply audio that was played, 0-100. */
  playedPercent?: number;
}
/** One line of the live conversation, kept by the bridge so surfaces never depend on host caches. */
export interface TranscriptEntry {
  role: "user" | "assistant";
  text: string;
}
export const TRANSCRIPT_LIMIT = 50;
export const TRANSCRIPT_TEXT_BYTES = 2000;
export interface Snapshot {
  status: VoiceStatus;
  /** Spoken output is paused. The runtime and in-flight work keep running. */
  paused: boolean;
  serverID: string | null;
  sessionID: string | null;
  /** Latest cut-off reply until the next prompt; null otherwise. */
  interrupted: Interruption | null;
  /** Latest spoken prompts, supervisor replies and worker updates; bounded. */
  transcript: TranscriptEntry[];
}
export interface BridgeOptions {
  client: OpenCodeClient;
  target: Target;
  settings: LocalSettings;
  /** Local package root used to install the packaged engine. */
  packageRoot?: string;
  /** Fallback when an older server does not return its parsed runtime options. */
  runtimeConfig?: RuntimeConfig;
  onChange(snapshot: Snapshot): void;
  onError(error: Error): void;
  runtime?: (options: RuntimeOptions) => Runtime;
}
interface Turn {
  number: number;
  inboxID?: string;
  delivered: boolean;
  deliveryDone: Promise<void>;
  resolveDelivery(): void;
  cancelled: boolean;
  execution?: Execution;
  pending: { event: OpenCodeEvent; generation: number; saturated?: boolean }[];
  pendingBytes: number;
  pendingTextBytes: number;
  saturatedReplies: Set<string>;
  evictedReplies: Set<string>;
}
interface Execution {
  started: boolean;
  finished: boolean;
  done: Promise<void>;
  end(): void;
}
interface Delivery {
  owner: { kind: "user"; turn: Turn } | { kind: "announcement"; inboxID: string };
  generation: number;
  execution: Execution;
}
interface Assistant {
  source: Delivery;
  reply?: {
    ordinal: number;
    ended: boolean;
    bytes: number;
    deltas: Set<string>;
    saturated: boolean;
  };
}
const MAX_TRACKED = 4096;
const CLEANUP_TIMEOUT_MS = 15_000;
export const MAX_REPLY_BYTES = 262_144;
export const MAX_ANNOUNCEMENT_BYTES = 8192;

class StoppedError extends Error {}
/** Internal adapter distinction: intentional local cancellation is not cleanup failure. */
export const isStoppedError = (error: unknown): error is Error =>
  error instanceof StoppedError ||
  (error instanceof Error && error.name === "AbortError" && error.cause instanceof StoppedError);

export function clipUtf8(text: string, limit: number): string {
  return clippedUtf8(text, limit).text;
}

function clippedUtf8(text: string, limit: number) {
  const budget = Math.max(0, Math.floor(limit));
  const prefix: string[] = [];
  let bytes = 0;
  let index = 0;
  // Work and allocation depend on the retained prefix, not discarded output.
  // Normalize C0 one-for-one and match Buffer UTF-8 replacement for lone surrogates.
  while (index < text.length && bytes < budget) {
    const code = text.charCodeAt(index);
    let character = text[index]!;
    let units = 1;
    let width = code < 128 ? 1 : code < 2048 ? 2 : 3;
    if (code < 32 && code !== 9 && code !== 10) character = " ";
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = text.charCodeAt(index + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        character = text.slice(index, index + 2);
        units = 2;
        width = 4;
      } else character = "\ufffd";
    } else if (code >= 0xdc00 && code <= 0xdfff) character = "\ufffd";
    if (bytes + width > budget) break;
    prefix.push(character);
    index += units;
    bytes += width;
  }
  return { text: prefix.join(""), bytes, truncated: index < text.length };
}

const rejectedPrompt = (error: unknown) =>
  isInvalidRequestError(error) ||
  isConflictError(error) ||
  isSessionNotFoundError(error) ||
  isLocationNotFoundError(error) ||
  isUnauthorizedError(error) ||
  isForbiddenError(error) ||
  isModelNotFoundError(error) ||
  isAgentNotFoundError(error);

function remember(set: Set<string>, id: string) {
  set.add(id);
  if (set.size > MAX_TRACKED) set.delete(set.values().next().value!);
}

function newExecution(started: boolean): Execution {
  let end!: () => void;
  const done = new Promise<void>((resolve) => {
    end = resolve;
  });
  return { started, finished: false, done, end };
}

function endExecution(execution: Execution | undefined) {
  if (!execution) return;
  execution.finished = true;
  execution.end();
}
const eventTypes = [
  "session.moved",
  "session.inbox.enqueued",
  "session.inbox.delivered",
  "session.execution.started",
  "session.step.started",
  "session.text.delta",
  "session.text.ended",
  "session.execution.succeeded",
  "session.execution.failed",
  "session.execution.interrupted",
] as const;

/** SDK plumbing only: Rust decides when to listen, interrupt, speak, or start a turn. */
export function createBridge(options: BridgeOptions) {
  // Freeze values, not host accessors that can silently change servers later.
  const client = options.client;
  const target: Target = { ...options.target };
  const rpc = client.rpc(Converse);
  const request = { location: { directory: target.directory } };
  const abort = new AbortController();
  let resolveStream!: () => void;
  let rejectStream!: (error: Error) => void;
  const streamReady = new Promise<void>((resolve, reject) => {
    resolveStream = resolve;
    rejectStream = reject;
  });
  void streamReady.catch(() => {});
  let runtime: Runtime | undefined;
  let runtimeReady = false;
  let sessionID: string | null = null;
  let status: VoiceStatus = "off";
  let paused = false;
  let closed = false;
  let started = false;
  let active: Turn | undefined;
  let lastTurn = 0;
  let lastSequence = 0;
  // Generation is local user activity, not a server timestamp (which may be skewed).
  let generation = 0;
  let execution: Execution | undefined;
  let delivery: Delivery | undefined;
  const inboxes = new Set<string>();
  const retiredInboxes = new Set<string>();
  const retiredAssistants = new Set<string>();
  const saturatedReplies = new Set<string>();
  const assistants = new Map<string, Assistant>();
  const users = new Map<string, Turn>();
  let pendingPrompt: Turn | undefined;
  let queueNextPrompt = false;
  let operation = Promise.resolve();
  const serialize = (run: () => Promise<void>) => {
    const next = operation.then(run);
    operation = next.catch(() => {});
    return next;
  };
  const retireInbox = (id: string) => {
    inboxes.delete(id);
    users.delete(id);
    remember(retiredInboxes, id);
  };
  const retireAssistants = () => {
    for (const id of assistants.keys()) remember(retiredAssistants, id);
    assistants.clear();
  };
  let cutOff: Interruption | null = null;
  const transcript: TranscriptEntry[] = [];
  const record = (role: TranscriptEntry["role"], text: string) => {
    const clipped = clippedUtf8(text, TRANSCRIPT_TEXT_BYTES);
    const line = clipped.truncated ? `${clipped.text.trimEnd()}…` : clipped.text.trim();
    if (!line) return;
    transcript.push({ role, text: line });
    if (transcript.length > TRANSCRIPT_LIMIT)
      transcript.splice(0, transcript.length - TRANSCRIPT_LIMIT);
    changed();
  };
  let stopping: Promise<void> | undefined;
  let nativeStopFailed = false;
  const snapshot = (): Snapshot => ({
    status,
    paused,
    serverID: sessionID ? target.serverID : null,
    sessionID,
    interrupted: cutOff,
    transcript: transcript.map((entry) => ({ ...entry })),
  });
  const changed = () => options.onChange(snapshot());
  const send = (message: HostMessage) => {
    if (!closed) runtime?.send(message);
  };
  const finish = (reason: Extract<HostMessage, { type: "turn_end" }>["reason"]) => {
    const current = active;
    if (!current) return;
    active = undefined;
    send({ type: "turn_end", turn: current.number, reason });
    if (!current.cancelled && current.inboxID) retireInbox(current.inboxID);
  };
  const stop = (): Promise<void> => {
    if (stopping && !nativeStopFailed) return stopping;
    let resolveStop!: () => void;
    let rejectStop!: (error: unknown) => void;
    // Publish before notifying off: a reentrant host cleanup shares this attempt.
    stopping = new Promise<void>((resolve, reject) => {
      resolveStop = resolve;
      rejectStop = reject;
    });
    nativeStopFailed = false;
    const cleanup = async () => {
      let nativeFailure: unknown;
      let failed = false;
      try {
        await runtime?.stop();
      } catch (error) {
        failed = true;
        nativeFailure = error;
      }
      try {
        // Refusal after movement is reported at the original location, not made
        // a permanent local restart barrier or worked around by retargeting.
        if (sessionID)
          await rpc.stop(
            { sessionID },
            { ...request, signal: AbortSignal.timeout(CLEANUP_TIMEOUT_MS) },
          );
      } catch (error) {
        options.onError(error instanceof Error ? error : new Error(String(error)));
      }
      if (failed) throw nativeFailure;
    };
    try {
      if (!closed) {
        try {
          finish("interrupted");
        } finally {
          closed = true;
          endExecution(execution);
          const stopped = new StoppedError("Converse stopped before the event stream connected");
          rejectStream(stopped);
          abort.abort(stopped);
          status = "off";
          paused = false;
          runtimeReady = false;
          changed();
        }
      }
    } finally {
      // Even a synchronous send/callback exception cannot strand the published
      // promise or prevent native/server cleanup from starting.
      void cleanup().then(resolveStop, (error) => {
        nativeStopFailed = true;
        rejectStop(error);
      });
    }
    return stopping;
  };
  const fail = (error: unknown) => {
    if (closed) return;
    const failure = error instanceof Error ? error : new Error(String(error));
    rejectStream(failure);
    options.onError(failure);
    finish("disconnected");
    void stop().catch(() => {});
  };
  const newActivity = () => {
    generation++;
    delivery = undefined;
  };
  const dispatch = (
    event: OpenCodeEvent,
    observedGeneration = generation,
    saturateDelta = false,
  ) => {
    if (closed || !sessionID || !("sessionID" in event.data) || event.data.sessionID !== sessionID)
      return;
    // A session ID survives movement. Never follow it to a different location,
    // even while a prompt response is pending or the event sequence looks stale.
    if (
      event.type === "session.moved" ||
      (event.location && event.location.directory !== target.directory)
    )
      return fail(
        new Error("Converse session moved from its selected location; start a new conversation"),
      );
    const current = active;
    if (pendingPrompt && !pendingPrompt.inboxID) {
      // Keep lifecycle/correlation events intact, but roll bounded text history.
      // Long output is not a disconnect, even when it overtakes the HTTP response.
      const textEvent = event.type === "session.text.delta" || event.type === "session.text.ended";
      if (event.type === "session.text.delta") {
        const reply = `${event.data.assistantMessageID}:${event.data.ordinal}`;
        if (pendingPrompt.saturatedReplies.has(reply)) return;
        const clipped = clippedUtf8(event.data.delta, MAX_REPLY_BYTES);
        event = {
          ...event,
          data: { ...event.data, delta: clipped.text },
        };
        saturateDelta = clipped.truncated;
        if (
          pendingPrompt.pendingTextBytes + Buffer.byteLength(JSON.stringify(event)) >
          MAX_REPLY_BYTES
        ) {
          // Retain a tiny ordered fence at the omission point, not a later suffix.
          event = { ...event, data: { ...event.data, delta: "" } };
          saturateDelta = true;
        }
        if (saturateDelta) remember(pendingPrompt.saturatedReplies, reply);
      }
      if (event.type === "session.text.ended")
        event = {
          ...event,
          data: { ...event.data, text: clipUtf8(event.data.text, MAX_REPLY_BYTES) },
        };
      const bytes = Buffer.byteLength(JSON.stringify(event));
      if (textEvent) {
        while (pendingPrompt.pendingTextBytes + bytes > MAX_REPLY_BYTES * 2) {
          const index = pendingPrompt.pending.findIndex(
            ({ event }) =>
              event.type === "session.text.delta" || event.type === "session.text.ended",
          );
          if (index < 0) break;
          const [removed] = pendingPrompt.pending.splice(index, 1);
          const size = Buffer.byteLength(JSON.stringify(removed!.event));
          pendingPrompt.pendingBytes -= size;
          pendingPrompt.pendingTextBytes -= size;
          if (removed!.event.type === "session.text.delta") {
            const data = removed!.event.data;
            const reply = `${data.assistantMessageID}:${data.ordinal}`;
            remember(pendingPrompt.saturatedReplies, reply);
            remember(pendingPrompt.evictedReplies, reply);
            // If the earliest buffered prefix is evicted, remaining deltas for
            // that reply cannot be emitted as a suffix. Completion still survives.
            pendingPrompt.pending = pendingPrompt.pending.filter((entry) => {
              if (
                entry.event.type !== "session.text.delta" ||
                `${entry.event.data.assistantMessageID}:${entry.event.data.ordinal}` !== reply
              )
                return true;
              const size = Buffer.byteLength(JSON.stringify(entry.event));
              pendingPrompt!.pendingBytes -= size;
              pendingPrompt!.pendingTextBytes -= size;
              return false;
            });
          }
        }
        if (
          event.type === "session.text.delta" &&
          pendingPrompt.evictedReplies.has(`${event.data.assistantMessageID}:${event.data.ordinal}`)
        )
          return;
        pendingPrompt.pendingTextBytes += bytes;
      }
      pendingPrompt.pendingBytes += bytes;
      if (pendingPrompt.pendingBytes > 1_048_576)
        return fail(new Error("Converse prompt event buffer exceeded limit"));
      pendingPrompt.pending.push({
        event,
        generation: observedGeneration,
        saturated: saturateDelta,
      });
      return;
    }
    const sequence = "durable" in event ? event.durable.seq : undefined;
    if (typeof sequence === "number") {
      // Session events have a durable total order. Late terminal/delivery events
      // must not change the owner of a newer execution, even with a different ID.
      if (sequence <= lastSequence) return;
      lastSequence = sequence;
    }
    if (event.type === "session.inbox.enqueued") {
      const { inboxID, item } = event.data;
      if (
        item.type === "synthetic" &&
        item.delivery === "queue" &&
        item.payload.metadata?.[WORKER_UPDATE_MARKER] === true &&
        !retiredInboxes.has(inboxID)
      ) {
        // Retiring old pending items is conservative: their later delivery stays
        // silent. Durable sequence fences prevent replay of retired enqueue events.
        remember(inboxes, inboxID);
      }
      return;
    }
    if (event.type === "session.execution.started") {
      if (execution && !execution.started && !execution.finished) {
        // Also tolerate hosts that publish delivery immediately before started.
        execution.started = true;
      } else {
        // A newly observed execution supersedes an unfinished older one. Do not
        // leave its user turn waiting for the new execution's terminal event.
        if (execution && !execution.finished && current?.execution === execution)
          finish("interrupted");
        endExecution(execution);
        retireAssistants();
        execution = newExecution(true);
        delivery = undefined;
      }
      return;
    }
    if (event.type === "session.inbox.delivered") {
      const inboxID = event.data.inboxID;
      if (retiredInboxes.has(inboxID)) return;
      const background = inboxes.has(inboxID);
      const user = users.get(inboxID);
      if (user?.delivered) return;
      delivery = undefined;
      if (!user && !background) return;
      execution ??= newExecution(false);
      if (execution.finished) execution = newExecution(false);
      if (user) {
        user.delivered = true;
        user.execution = execution;
        user.resolveDelivery();
        delivery = {
          owner: { kind: "user", turn: user },
          generation: observedGeneration,
          execution,
        };
      } else if (background) {
        retireInbox(inboxID);
        delivery = {
          owner: { kind: "announcement", inboxID },
          generation: observedGeneration,
          execution,
        };
      }
      return;
    }
    if (event.type === "session.step.started") {
      const id = event.data.assistantMessageID;
      if (!delivery || delivery.generation !== generation || delivery.execution.finished) return;
      if (assistants.has(id) || retiredAssistants.has(id)) return;
      retireAssistants();
      assistants.set(id, { source: delivery });
      return;
    }
    if (event.type === "session.text.delta" || event.type === "session.text.ended") {
      const data = event.data;
      const assistant = assistants.get(data.assistantMessageID);
      const source = assistant?.source;
      if (!source || source.generation !== generation || source.execution.finished) return;
      if (!assistant.reply || data.ordinal > assistant.reply.ordinal) {
        assistant.reply = {
          ordinal: data.ordinal,
          ended: false,
          bytes: 0,
          deltas: new Set(),
          saturated: saturatedReplies.has(`${data.assistantMessageID}:${data.ordinal}`),
        };
      }
      const state = assistant.reply;
      if (data.ordinal !== state.ordinal || state.ended) return;
      const reply = `${data.assistantMessageID}:${data.ordinal}`;
      if (source.owner.kind === "user") {
        if (active !== source.owner.turn) return;
        if (event.type === "session.text.delta") {
          if (state.saturated || state.deltas.has(event.id)) return;
          state.deltas.add(event.id);
          const clipped = clippedUtf8(event.data.delta, MAX_REPLY_BYTES - state.bytes);
          const text = clipped.text;
          state.bytes += clipped.bytes;
          // Exact dedupe remains bounded even for an indefinitely streamed reply.
          // After this prefix, use the authoritative full-text completion, rather
          // than evict IDs and risk speaking a replay of an old delta.
          state.saturated =
            saturateDelta ||
            clipped.truncated ||
            state.bytes >= MAX_REPLY_BYTES ||
            state.deltas.size >= MAX_TRACKED;
          if (text) send({ type: "reply_delta", turn: active.number, reply, text });
        } else {
          // Full text is always sent, including for providers with no deltas.
          record("assistant", event.data.text);
          send({
            type: "reply_end",
            turn: active.number,
            reply,
            text: clipUtf8(event.data.text, MAX_REPLY_BYTES),
          });
        }
      } else if (event.type === "session.text.ended") {
        // Rust alone decides whether to queue, speak, or drop this complete text.
        record("assistant", event.data.text);
        send({
          type: "announcement",
          reply,
          text: clipUtf8(event.data.text, MAX_ANNOUNCEMENT_BYTES),
        });
      }
      if (event.type === "session.text.ended") {
        state.ended = true;
        state.deltas.clear();
      }
      return;
    }
    if (
      event.type === "session.execution.succeeded" ||
      event.type === "session.execution.failed" ||
      event.type === "session.execution.interrupted"
    ) {
      if (!execution || execution.finished) return;
      endExecution(execution);
      retireAssistants();
      delivery = undefined;
      if (!current?.delivered || current.execution !== execution) return;
      if (event.type === "session.execution.succeeded") finish("completed");
      if (event.type === "session.execution.failed") finish("failed");
      if (event.type === "session.execution.interrupted") finish("interrupted");
    }
  };
  const consume = async () => {
    try {
      for await (const event of client.event.subscribe({
        signal: abort.signal,
        types: [...eventTypes],
      })) {
        // The SDK always includes server.connected, including on an already
        // shared connection. Do not let a fast local runtime prompt before the
        // authenticated stream is actually live (especially against remote hosts).
        if (event.type === "server.connected") resolveStream();
        else dispatch(event);
      }
      if (!closed)
        fail(
          new Error("Converse server event stream closed; start a new conversation to reconnect"),
        );
    } catch (error) {
      if (!closed) fail(error);
    }
  };
  const replayPending = (turn: Turn) => {
    const pending = turn.pending;
    turn.pending = [];
    turn.pendingBytes = 0;
    turn.pendingTextBytes = 0;
    // Evicted prefixes must be fenced before replay; ordinary omission fences
    // remain ordered after the valid retained prefix instead of suppressing it.
    for (const reply of turn.evictedReplies) remember(saturatedReplies, reply);
    for (const [id, assistant] of assistants) {
      if (assistant.reply && turn.evictedReplies.has(`${id}:${assistant.reply.ordinal}`))
        assistant.reply.saturated = true;
    }
    for (const entry of pending) dispatch(entry.event, entry.generation, entry.saturated);
    for (const reply of turn.saturatedReplies) remember(saturatedReplies, reply);
    for (const [id, assistant] of assistants) {
      if (assistant.reply && saturatedReplies.has(`${id}:${assistant.reply.ordinal}`))
        assistant.reply.saturated = true;
    }
    turn.saturatedReplies.clear();
    turn.evictedReplies.clear();
  };
  const requestOptions = () => ({
    signal: AbortSignal.any([abort.signal, AbortSignal.timeout(10_000)]),
  });
  const waitForConfirmation = (done: Promise<void>, timeoutMessage: string) =>
    new Promise<void>((resolve, reject) => {
      const cleanup = () => {
        clearTimeout(timer);
        abort.signal.removeEventListener("abort", cancelled);
      };
      const cancelled = () => {
        cleanup();
        reject(new Error("Converse stopped"));
      };
      const timer = setTimeout(() => {
        cleanup();
        reject(new Error(timeoutMessage));
      }, 10_000);
      abort.signal.addEventListener("abort", cancelled, { once: true });
      void done.then(() => {
        cleanup();
        resolve();
      });
      if (abort.signal.aborted) cancelled();
    });
  const waitForExecution = (execution: Execution) =>
    waitForConfirmation(execution.done, "Converse interrupt completion timed out");
  const submit = async (turn: Turn, text: string) => {
    if (closed || turn.cancelled || !sessionID) return;
    pendingPrompt = turn;
    try {
      const delivery = queueNextPrompt ? ("queue" as const) : undefined;
      queueNextPrompt = false;
      const result = await client.session.prompt(
        { sessionID, text, ...(delivery ? { delivery } : {}) },
        requestOptions(),
      );
      turn.inboxID = result.id;
      users.set(result.id, turn);
    } catch (error) {
      if (!closed && !rejectedPrompt(error)) {
        // A lost response does not prove that the server rejected admission,
        // even if Rust already cancelled this turn while awaiting its inbox ID.
        fail(
          new Error("Converse prompt admission is unconfirmed; start a new conversation", {
            cause: error,
          }),
        );
      } else if (!closed && active === turn && !turn.cancelled) {
        send({
          type: "prompt_failed",
          turn: turn.number,
          message:
            error instanceof Error || rejectedPrompt(error)
              ? (error as { message: string }).message
              : "Prompt failed",
        });
        active = undefined;
      }
    } finally {
      pendingPrompt = undefined;
      // Even failed or preempted prompt requests can overlap valid autonomous
      // worker updates. Replay those with their original receipt generations.
      replayPending(turn);
    }
  };
  const cancel = async (turn: Turn) => {
    if (closed || !sessionID || !turn.inboxID) return;
    try {
      const interruptUnfinished = async () => {
        if (
          !closed &&
          turn.delivered &&
          turn.execution &&
          turn.execution === execution &&
          !turn.execution.finished
        )
          await client.session.interrupt({ sessionID: sessionID! }, requestOptions());
      };
      if (!turn.delivered) {
        try {
          await client.session.inbox.cancel({ sessionID, inboxID: turn.inboxID }, requestOptions());
        } catch (error) {
          // Delivery can win the race with cancellation of a queued inbox item.
          if (!turn.delivered) {
            // A typed request rejection can overtake the delivery SSE event.
            // It is not itself proof of delivery: retain the inbox/barrier until
            // the event confirms the owner, or fail closed at the deadline.
            if (!isInvalidRequestError(error)) throw error;
            await waitForConfirmation(
              turn.deliveryDone,
              "Converse inbox delivery confirmation timed out",
            );
          }
        }
      }
      await interruptUnfinished();
      // An interrupt response acknowledges cancellation, not necessarily its
      // terminal SSE event. Keep the next prompt behind that old execution too.
      if (turn.delivered && turn.execution && !turn.execution.finished)
        await waitForExecution(turn.execution);
    } catch (error) {
      // Neither an RPC failure nor a timeout confirms that the old work stopped.
      // Closing synchronously prevents the serialized replacement from submitting.
      fail(error);
    } finally {
      retireInbox(turn.inboxID);
    }
  };
  const onMessage = (message: EngineMessage) => {
    if (closed) return;
    if (message.type === "status") {
      if (
        status !== message.state &&
        (message.state === "hearing" || message.state === "muted" || status === "muted")
      )
        newActivity();
      status = message.state;
      changed();
    }
    if (message.type === "error" && !message.fatal) options.onError(new Error(message.message));
    if (message.type === "prompt") {
      if (!sessionID || message.turn <= lastTurn) return;
      if (active) {
        send({
          type: "prompt_failed",
          turn: message.turn,
          message: "Previous turn is still active",
        });
        return;
      }
      lastTurn = message.turn;
      if (cutOff) {
        cutOff = null;
        changed();
      }
      newActivity();
      record("user", message.text);
      let resolveDelivery!: () => void;
      const deliveryDone = new Promise<void>((resolve) => {
        resolveDelivery = resolve;
      });
      const current: Turn = {
        number: message.turn,
        delivered: false,
        deliveryDone,
        resolveDelivery,
        cancelled: false,
        pending: [],
        pendingBytes: 0,
        pendingTextBytes: 0,
        saturatedReplies: new Set(),
        evictedReplies: new Set(),
      };
      active = current;
      void serialize(() => submit(current, message.text));
    }
    if (message.type === "interrupt" && active?.number === message.turn && sessionID) {
      const interrupted = active;
      interrupted.cancelled = true;
      cutOff = {
        ...(message.heard === undefined ? {} : { heard: message.heard }),
        ...(message.played_ms === undefined || !message.generated_ms
          ? {}
          : { playedPercent: Math.round((message.played_ms / message.generated_ms) * 100) }),
      };
      changed();
      queueNextPrompt = true;
      newActivity();
      finish("interrupted");
      void serialize(() => cancel(interrupted));
    }
  };
  return {
    snapshot,
    async start() {
      if (started || closed) throw new Error("Converse bridge already started or stopped");
      started = true;
      status = "loading";
      changed();
      try {
        const hello = await waitForConverse(
          client,
          target.directory,
          (signal) => rpc.hello(undefined, { ...request, signal }),
          abort.signal,
        );
        if (hello.protocol !== 1) throw new Error("Converse server protocol mismatch");
        if (closed) return;
        const result = await rpc.start(undefined, {
          ...request,
          // Allocation must complete (or reach its own deadline) even if local
          // stop runs, so a late returned server session can always be cleaned up.
          signal: AbortSignal.timeout(10_000),
        });
        sessionID = result.sessionID;
        const runtimeConfig = result.runtime ?? options.runtimeConfig;
        if (closed) {
          try {
            await rpc.stop(
              { sessionID },
              { ...request, signal: AbortSignal.timeout(CLEANUP_TIMEOUT_MS) },
            );
          } catch (error) {
            throw new Error("Late Converse session cleanup failed at the selected location", {
              cause: error,
            });
          }
          return;
        }
        changed();
        void consume();
        const deadline = setTimeout(
          () => rejectStream(new Error("Converse server event stream handshake timed out")),
          10_000,
        );
        try {
          await streamReady;
        } finally {
          clearTimeout(deadline);
        }
        if (closed) return;
        runtime = (options.runtime ?? startRuntime)({
          settings: options.settings,
          ...(options.packageRoot ? { packageRoot: options.packageRoot } : {}),
          ...(runtimeConfig ? { runtime: runtimeConfig } : {}),
          onMessage,
          onError: fail,
        });
        if (closed) {
          await runtime.stop();
          return;
        }
        try {
          await runtime.ready;
        } catch (error) {
          if (closed && isStoppedError(abort.signal.reason)) throw abort.signal.reason;
          throw error;
        }
        runtimeReady = true;
        if (paused) send({ type: "pause", on: true });
      } catch (error) {
        fail(error);
        await stop().catch(() => {});
        throw error;
      }
    },
    stop,
    /** Point the supervisor at an existing session inside this conversation's location. */
    async attach(targetSessionID: string) {
      if (closed || !sessionID || !runtimeReady) throw new Error("Converse is not running");
      await rpc.attach(
        { supervisorID: sessionID, sessionID: targetSessionID },
        { ...request, signal: AbortSignal.any([abort.signal, AbortSignal.timeout(15_000)]) },
      );
    },
    /** Stop following a session; the session and its running work are untouched. */
    async detach(targetSessionID: string) {
      if (closed || !sessionID || !runtimeReady) throw new Error("Converse is not running");
      await rpc.detach(
        { supervisorID: sessionID, sessionID: targetSessionID },
        { ...request, signal: AbortSignal.any([abort.signal, AbortSignal.timeout(15_000)]) },
      );
    },
    mute() {
      if (status !== "muted") newActivity();
      send({ type: "mute", on: status !== "muted" });
    },
    pause(on: boolean) {
      if (closed || paused === on) return;
      paused = on;
      if (runtimeReady) send({ type: "pause", on });
      changed();
    },
  };
}
