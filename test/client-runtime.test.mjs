import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { posix, win32 } from "node:path";
import { PassThrough, Writable } from "node:stream";
import test from "node:test";
import { MAX_FRAME_BYTES } from "../dist/protocol.js";
import { engineExecutable, RUNTIME_VERSION, stateDir } from "../dist/client/engine.js";
import {
  localSettings,
  parseEngineMessage,
  runtimeEnvironment,
  runtimeSettings,
  startRuntime,
} from "../dist/client/runtime.js";

function child() {
  const proc = new EventEmitter();
  proc.stdout = new PassThrough();
  proc.stderr = new PassThrough();
  proc.frames = [];
  proc.stdin = new Writable({
    write(chunk, _encoding, done) {
      proc.frames.push(JSON.parse(chunk.toString()));
      done();
    },
  });
  proc.kills = [];
  proc.kill = (signal) => {
    proc.kills.push(signal);
    proc.emit("close", null, signal);
    return true;
  };
  proc.frame = (value) => proc.stdout.write(JSON.stringify(value) + "\n");
  return proc;
}
function setup(extra = {}) {
  const proc = child();
  const messages = [];
  const errors = [];
  const runtime = startRuntime({
    settings: { inputDevice: "mic", executable: "/bad" },
    spawn: () => proc,
    onMessage: (value) => messages.push(value),
    onError: (error) => errors.push(error),
    shutdownTimeoutMs: 2,
    helloTimeoutMs: 100,
    ...extra,
  });
  return { proc, runtime, messages, errors };
}

const DEFAULT_ROSTER = [{ id: "a1", name: "Converse", voice: "default" }];

test("runtime sends only protocol init and local device preferences", async () => {
  const { proc, runtime, messages } = setup();
  proc.stdout.write('{"type":"hello","pro');
  proc.stdout.write('tocol":3}\n');
  await runtime.ready;
  assert.deepEqual(proc.frames, [
    { type: "init", protocol: 3, settings: { inputDevice: "mic", participants: DEFAULT_ROSTER } },
  ]);
  proc.frame({ type: "status", state: "listening" });
  assert.deepEqual(messages, [{ type: "status", state: "listening" }]);
  runtime.send({ type: "mute", on: true });
  const a = runtime.stop();
  assert.equal(runtime.stop(), a);
  await a;
  assert.equal(proc.stdin.writableEnded, true);
  assert.deepEqual(proc.frames.at(-1), { type: "shutdown" });
  assert.deepEqual(proc.kills, ["SIGKILL"]);
});

test("runtime sends configured inference tuning over init", async () => {
  const { proc, runtime } = setup({
    runtime: { asrThreads: 2, ttsThreads: 3 },
  });
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  assert.deepEqual(proc.frames, [
    {
      type: "init",
      protocol: 3,
      settings: {
        inputDevice: "mic",
        asrThreads: 2,
        ttsThreads: 3,
        participants: DEFAULT_ROSTER,
      },
    },
  ]);
  await runtime.stop();
});

test("runtime sends endpointing over init", async () => {
  const { proc, runtime } = setup({ runtime: { endpointing: "patient" } });
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  assert.equal(proc.frames[0].settings.endpointing, "patient");
  await runtime.stop();
});

test("runtime sends echoCancellation over init", async () => {
  const { proc, runtime } = setup({ runtime: { echoCancellation: "off" } });
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  assert.equal(proc.frames[0].settings.echoCancellation, "off");
  await runtime.stop();
});

test("runtime sends agentChat over init and parses relayed prompts and chain ends", async () => {
  const { proc, runtime, messages } = setup({ runtime: { agentChat: "off" } });
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  assert.equal(proc.frames[0].settings.agentChat, "off");
  const relayed = {
    type: "prompt",
    turn: 2,
    speaker: "a2",
    addressed: true,
    text: "x",
    from: "a1",
  };
  proc.frame(relayed);
  proc.frame({ type: "chain_end", turn: 4, reason: "ping_pong" });
  assert.deepEqual(messages, [relayed, { type: "chain_end", turn: 4, reason: "ping_pong" }]);
  await runtime.stop();
  for (const bad of [
    { ...relayed, from: "not valid!" },
    { type: "chain_end", turn: 4, reason: "because" },
    { type: "chain_end", turn: 4, reason: "turns", extra: 1 },
  ]) {
    const s = setup();
    s.proc.frame({ type: "hello", protocol: 3 });
    await s.runtime.ready;
    s.proc.frame(bad);
    assert.equal(s.messages.length, 0);
    await s.runtime.stop().catch(() => {});
  }
});

test("runtime sends acknowledgement over init", async () => {
  const { proc, runtime } = setup({ runtime: { acknowledgement: "off" } });
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  assert.equal(proc.frames[0].settings.acknowledgement, "off");
  await runtime.stop();
});

test("runtimeSettings contains only inference tuning", () => {
  assert.deepEqual(runtimeSettings({ asrThreads: 2 }), {
    asrThreads: 2,
    participants: DEFAULT_ROSTER,
  });
});

test("fragmented valid stdout frames are concatenated at most once per completed frame", async (t) => {
  const s = setup();
  s.proc.frame({ type: "hello", protocol: 3 });
  await s.runtime.ready;
  const text = "x".repeat(MAX_FRAME_BYTES - 100);
  const frame = Buffer.from(
    JSON.stringify({ type: "prompt", turn: 1, speaker: "a1", addressed: false, text }) + "\n",
  );
  let concatenations = 0;
  let copied = 0;
  const original = Buffer.concat;
  const mock = t.mock.method(Buffer, "concat", (parts, length) => {
    concatenations++;
    copied += length ?? parts.reduce((bytes, part) => bytes + part.length, 0);
    return original(parts, length);
  });
  for (let offset = 0; offset < frame.length; offset += 4096)
    s.proc.stdout.write(frame.subarray(offset, offset + 4096));
  mock.mock.restore();
  assert.equal(s.messages.length, 1);
  assert.ok(s.messages[0].text === text);
  assert.equal(concatenations, 1, "fragmentation must not repeatedly copy the retained prefix");
  assert.equal(copied, frame.length - 1);
  s.proc.stdout.write(
    '{"type":"status","state":"thinking"}\n{"type":"status","state":"speaking"}\n',
  );
  assert.deepEqual(s.messages.slice(1), [
    { type: "status", state: "thinking" },
    { type: "status", state: "speaking" },
  ]);
  await s.runtime.stop();
});

test("stdout chunk accumulation preserves split UTF-8 and coalesced frame suffixes", async () => {
  const s = setup();
  s.proc.frame({ type: "hello", protocol: 3 });
  await s.runtime.ready;
  const first = Buffer.from(
    '{"type":"prompt","turn":1,"speaker":"a1","addressed":false,"text":"€😀"}\n',
  );
  const split = first.indexOf(Buffer.from("😀")) + 1;
  s.proc.stdout.write(first.subarray(0, split));
  s.proc.stdout.write(
    Buffer.concat([first.subarray(split), Buffer.from('{"type":"status","state":')]),
  );
  s.proc.stdout.write('"thinking"}\n');
  assert.deepEqual(s.messages, [
    { type: "prompt", turn: 1, speaker: "a1", addressed: false, text: "€😀" },
    { type: "status", state: "thinking" },
  ]);
  assert.deepEqual(s.errors, []);
  await s.runtime.stop();
});

test("stop waits for confirmed close after SIGKILL and rejects unconfirmed termination", async () => {
  for (const outcome of ["delayed", "false", "throw", "no-close"]) {
    const s = setup({ shutdownTimeoutMs: 5, killTimeoutMs: 50 });
    s.proc.frame({ type: "hello", protocol: 3 });
    await s.runtime.ready;
    let signalled;
    const signal = new Promise((resolve) => {
      signalled = resolve;
    });
    s.proc.kill = () => {
      signalled();
      if (outcome === "throw") throw new Error("signal failed");
      return outcome !== "false";
    };
    let settled = false;
    const stopped = s.runtime.stop();
    void stopped.then(
      () => {
        settled = true;
      },
      () => {
        settled = true;
      },
    );
    await signal;
    await new Promise((resolve) => setImmediate(resolve));
    if (outcome === "delayed") {
      assert.equal(settled, false, "signalling is not process termination");
      s.proc.emit("close", null, "SIGKILL");
      await stopped;
    } else {
      await assert.rejects(stopped, /termination.*unconfirmed|signal failed/i);
    }
  }
});

test("failed kill remains blocking until late close makes a later stop successful", async () => {
  const s = setup({ shutdownTimeoutMs: 2, killTimeoutMs: 2 });
  s.proc.frame({ type: "hello", protocol: 3 });
  await s.runtime.ready;
  s.proc.kill = () => false;
  await assert.rejects(s.runtime.stop(), /termination is unconfirmed/);
  await assert.rejects(s.runtime.stop(), /termination is unconfirmed/);
  s.proc.emit("close", null, "SIGKILL");
  await s.runtime.stop();
});

test("outbound strings are bounded before JSON serialization or buffer allocation", async (t) => {
  for (const field of ["text", "reply", "settings"]) {
    const s = setup();
    s.proc.frame({ type: "hello", protocol: 3 });
    await s.runtime.ready;
    const huge = "x".repeat(MAX_FRAME_BYTES * 8);
    const message =
      field === "settings"
        ? { type: "init", protocol: 3, settings: { inputDevice: huge } }
        : {
            type: "reply_end",
            speaker: "a1",
            turn: 1,
            reply: field === "reply" ? huge : "r",
            text: field === "text" ? huge : "ok",
          };
    const original = JSON.stringify;
    let serialized = false;
    const mock = t.mock.method(JSON, "stringify", (value, ...args) => {
      if (value?.type === message.type) serialized = true;
      return original(value, ...args);
    });
    s.runtime.send(message);
    mock.mock.restore();
    assert.equal(serialized, false, `${field} must be rejected before serializing`);
    assert.match(s.errors[0].message, /exceeds limit/);
    await s.runtime.stop();
  }
});

test("bounded control-heavy speech is substituted one-for-one before JSON escaping", async () => {
  const s = setup();
  s.proc.frame({ type: "hello", protocol: 3 });
  await s.runtime.ready;
  s.runtime.send({
    type: "reply_end",
    speaker: "a1",
    turn: 1,
    reply: "r",
    text: "\t\n\r" + "\u0001".repeat(200_000),
  });
  assert.deepEqual(s.errors, []);
  assert.ok(s.proc.frames.at(-1).text === "\t\n " + " ".repeat(200_000));
  await s.runtime.stop();
});

for (const [name, frame, pattern] of [
  ["version mismatch", '{"type":"hello","protocol":1}\n', /version mismatch/],
  ["malformed JSON", "nope\n", /Unexpected|JSON/],
  ["unknown fields", '{"type":"hello","protocol":3,"token":"secret"}\n', /version mismatch/],
  ["wrong first message", '{"type":"status","state":"listening"}\n', /hello first/],
  ["oversized partial frame", "x".repeat(MAX_FRAME_BYTES + 1), /exceeds limit/],
  ["oversized terminated frame", "x".repeat(MAX_FRAME_BYTES + 1) + "\n", /exceeds limit/],
])
  test(`runtime rejects ${name}`, async () => {
    const { proc, runtime, errors } = setup();
    proc.stdout.write(frame);
    await assert.rejects(runtime.ready, pattern);
    await runtime.stop();
    assert.equal(errors.length, 1);
  });

test("strict validator rejects coercions, invalid turns and extra settings", () => {
  for (const value of [
    null,
    [],
    { type: "status", state: ["listening"] },
    { type: "prompt", turn: 0, speaker: "a1", addressed: false, text: "a" },
    { type: "prompt", turn: 1.1, speaker: "a1", addressed: false, text: "a" },
    { type: "prompt", turn: 1, speaker: "a1", addressed: false, text: " " },
    { type: "interrupt", turn: 1, extra: true },
    { type: "interrupt", turn: 1, played_ms: 5 },
    { type: "interrupt", turn: 1, played_ms: 9, generated_ms: 5 },
    { type: "interrupt", turn: 1, played_ms: -1, generated_ms: 5 },
    { type: "interrupt", turn: 1, played_ms: 1.5, generated_ms: 5 },
    { type: "interrupt", turn: 1, heard: 5 },
    { type: "interrupt", turn: 1, heard: "x".repeat(4097) },
  ])
    assert.throws(() => parseEngineMessage(value));
  for (const value of [
    { type: "interrupt", turn: 1 },
    { type: "interrupt", turn: 1, played_ms: 900, generated_ms: 2400 },
    { type: "interrupt", turn: 1, played_ms: 900, generated_ms: 2400, heard: "Hello there" },
  ])
    assert.deepEqual(parseEngineMessage(value), value);
  assert.deepEqual(
    localSettings({ inputDevice: "a", outputDevice: "b", binary: "evil", model: "evil" }),
    { inputDevice: "a", outputDevice: "b" },
  );
});

test("local device settings use the native UTF-8 byte and Unicode-control contract", () => {
  const exact = "€".repeat(341) + "a";
  assert.equal(Buffer.byteLength(exact), 1024);
  assert.deepEqual(localSettings({ inputDevice: exact, outputDevice: "valid" }), {
    inputDevice: exact,
    outputDevice: "valid",
  });
  for (const invalid of [exact + "b", "é".repeat(600), "€".repeat(342)])
    assert.deepEqual(localSettings({ inputDevice: invalid, outputDevice: "valid" }), {
      outputDevice: "valid",
    });
  for (const control of ["\u0000", "\u0001", "\t", "\n", "\r", "\u007f", "\u0085"])
    assert.deepEqual(
      localSettings({ inputDevice: "valid", outputDevice: `device${control}name` }),
      { inputDevice: "valid" },
    );
});

test("runtime reports EOF after handshake without accepting more events", async () => {
  const { proc, runtime, errors } = setup();
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  proc.stdout.end();
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(errors[0].message, /stdout closed/);
  await runtime.stop();
});

test("runtime handles process errors and handshake deadline", async () => {
  const missing = setup();
  missing.proc.emit("error", new Error("ENOENT"));
  await assert.rejects(missing.runtime.ready, /installed engine could not be started/);
  await missing.runtime.stop();
  const timeout = setup({ helloTimeoutMs: 1 });
  await assert.rejects(timeout.runtime.ready, /timed out/);
  await timeout.runtime.stop();
});

for (const name of ["stdout", "stderr", "stdin"])
  test(`runtime handles ${name} stream errors as fatal`, async () => {
    const { proc, runtime, errors } = setup();
    proc.frame({ type: "hello", protocol: 3 });
    await runtime.ready;
    proc[name].emit("error", new Error(`${name} failed`));
    assert.equal(errors.length, 1);
    assert.match(errors[0].message, new RegExp(`${name} failed`));
    await runtime.stop();
    assert.equal(proc.stdin.writableEnded, true);
    assert.deepEqual(proc.kills, ["SIGKILL"]);
  });

test("outbound oversized frames and blocked write queues fail and clean up without throwing", async () => {
  for (const blocked of [false, true]) {
    const { proc, runtime, errors } = setup();
    proc.frame({ type: "hello", protocol: 3 });
    await runtime.ready;
    if (blocked)
      Object.defineProperty(proc.stdin, "writableLength", { value: MAX_FRAME_BYTES * 2 });
    assert.doesNotThrow(() =>
      runtime.send({
        type: "reply_delta",
        speaker: "a1",
        turn: 1,
        reply: "assistant:0",
        text: blocked ? "a" : "a".repeat(MAX_FRAME_BYTES),
      }),
    );
    assert.equal(errors.length, 1);
    assert.match(errors[0].message, /outgoing frame\/queue exceeds limit/);
    await runtime.stop();
    assert.equal(proc.stdin.writableEnded, true);
    assert.deepEqual(proc.kills, ["SIGKILL"]);
  }
});

test("graceful exit avoids kill; duplicate hello and invalid UTF-8 fail", async () => {
  const graceful = setup();
  graceful.proc.frame({ type: "hello", protocol: 3 });
  await graceful.runtime.ready;
  const stopped = graceful.runtime.stop();
  graceful.proc.emit("close", 0, null);
  await stopped;
  assert.deepEqual(graceful.proc.kills, []);
  const duplicate = setup();
  duplicate.proc.frame({ type: "hello", protocol: 3 });
  await duplicate.runtime.ready;
  duplicate.proc.frame({ type: "hello", protocol: 3 });
  assert.match(duplicate.errors[0].message, /Duplicate/);
  await duplicate.runtime.stop();
  const invalid = setup();
  invalid.proc.stdout.write(Buffer.from([0xff, 10]));
  await assert.rejects(invalid.runtime.ready, /encoded data/);
  await invalid.runtime.stop();
});

test("state folder and engine path are fixed per platform", () => {
  assert.equal(
    stateDir({ env: {}, platform: "darwin", home: "/home/me" }),
    posix.join("/home/me", "Library", "Application Support", "opencode-converse"),
  );
  assert.equal(
    engineExecutable({ env: {}, platform: "darwin", home: "/home/me" }),
    posix.join(
      "/home/me",
      "Library",
      "Application Support",
      "opencode-converse",
      "bin",
      RUNTIME_VERSION,
      "opencode-converse",
    ),
  );
  assert.equal(
    stateDir({ env: { XDG_DATA_HOME: "/data" }, platform: "linux", home: "/home/me" }),
    "/data/opencode-converse",
  );
  assert.equal(
    stateDir({ env: { XDG_DATA_HOME: "relative" }, platform: "linux", home: "/home/me" }),
    posix.join("/home/me", ".local", "share", "opencode-converse"),
  );
  assert.equal(
    engineExecutable({ env: {}, platform: "win32", home: "C:\\Users\\me" }),
    win32.join(
      "C:\\Users\\me",
      "AppData",
      "Local",
      "opencode-converse",
      "bin",
      RUNTIME_VERSION,
      "opencode-converse.exe",
    ),
  );
  assert.equal(
    stateDir({
      env: { LOCALAPPDATA: "C:\\Users\\me\\AppData\\Local" },
      platform: "win32",
      home: "C:\\Users\\me",
    }),
    "C:\\Users\\me\\AppData\\Local\\opencode-converse",
  );
  for (const LOCALAPPDATA of ["", "relative", "C:relative", "\\relative-to-drive"])
    assert.equal(
      stateDir({ env: { LOCALAPPDATA }, platform: "win32", home: "C:\\Users\\me" }),
      win32.join("C:\\Users\\me", "AppData", "Local", "opencode-converse"),
    );
  assert.throws(() => stateDir({ env: {}, platform: "win32", home: "relative-home" }), /absolute/);
});

test("runtime environment drops credentials and keeps state-folder inputs", () => {
  assert.deepEqual(
    runtimeEnvironment({
      HOME: "/home/me",
      XDG_DATA_HOME: "/data",
      OPENCODE_SERVER_PASSWORD: "secret",
      OPENAI_API_KEY: "secret",
      NODE_OPTIONS: "evil",
      DYLD_INSERT_LIBRARIES: "evil",
      PATH: "/untrusted",
      XDG_RUNTIME_DIR: "/run/user/1000",
      PULSE_SERVER: "unix:/mnt/wslg/PulseServer",
      PIPEWIRE_REMOTE: "pipewire-0",
      PIPEWIRE_RUNTIME_DIR: "/run/user/1000/pipewire",
    }),
    {
      HOME: "/home/me",
      XDG_DATA_HOME: "/data",
      XDG_RUNTIME_DIR: "/run/user/1000",
      PULSE_SERVER: "unix:/mnt/wslg/PulseServer",
      PIPEWIRE_REMOTE: "pipewire-0",
      PIPEWIRE_RUNTIME_DIR: "/run/user/1000/pipewire",
    },
  );
});

test("runtime sends the configured roster, assigning unused voices in order", async () => {
  const { proc, runtime } = setup({
    runtime: { agents: [{ name: "Ada", voice: "easton" }, { name: "Grace" }, { name: "Linus" }] },
  });
  proc.frame({ type: "hello", protocol: 3 });
  await runtime.ready;
  assert.deepEqual(proc.frames[0].settings.participants, [
    { id: "a1", name: "Ada", voice: "easton" },
    { id: "a2", name: "Grace", voice: "zoey" },
    { id: "a3", name: "Linus", voice: "maeve" },
  ]);
  await runtime.stop();
});

test("engine frames carry validated speakers", async () => {
  const { parseEngineMessage } = await import("../dist/client/runtime.js");
  assert.deepEqual(parseEngineMessage({ type: "status", state: "speaking", speaker: "a2" }), {
    type: "status",
    state: "speaking",
    speaker: "a2",
  });
  assert.deepEqual(parseEngineMessage({ type: "interrupt", turn: 1, speaker: "a1" }), {
    type: "interrupt",
    turn: 1,
    speaker: "a1",
  });
  for (const bad of [
    { type: "status", state: "speaking", speaker: "a 2" },
    { type: "status", state: "speaking", speaker: 2 },
    { type: "prompt", turn: 1, addressed: false, text: "x" },
    { type: "prompt", turn: 1, speaker: "a1", text: "x" },
    { type: "prompt", turn: 1, speaker: "../x", addressed: false, text: "x" },
  ])
    assert.throws(() => parseEngineMessage(bad), /Invalid Converse frame/, JSON.stringify(bad));
});
