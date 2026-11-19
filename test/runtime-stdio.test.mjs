import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test, { before } from "node:test";
import { buildFixture } from "./client-cargo-fixture.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
let executable;
const init = { type: "init", protocol: 3, settings: {} };

async function cleanupResources(resources, remove, timeoutMs = 5000) {
  const results = await Promise.allSettled(
    [...resources.children].map(async ({ child, closed }) => {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      let timer;
      try {
        await Promise.race([
          closed,
          new Promise((_, reject) => {
            timer = setTimeout(
              () => reject(new Error("Runtime termination was not confirmed")),
              timeoutMs,
            );
          }),
        ]);
      } finally {
        clearTimeout(timer);
      }
    }),
  );
  const errors = results
    .filter((result) => result.status === "rejected")
    .map((result) => result.reason);
  if (errors.length)
    throw new AggregateError(
      errors,
      `Unconfirmed runtime termination; preserving storage: ${[...resources.directories].join(", ")}`,
    );
  for (const dir of resources.directories) await remove(dir);
}

const cleanupContexts = new WeakMap();
function resources(t) {
  let state = cleanupContexts.get(t);
  if (!state) {
    state = { directories: new Set(), children: new Set() };
    cleanupContexts.set(t, state);
    t.after(() => cleanupResources(state, (dir) => rm(dir, { recursive: true, force: true })));
  }
  return state;
}

test("runtime fixture cleanup confirms every shared child before removing storage", async () => {
  let terminated = 0;
  const children = [1, 2].map(() => {
    let finish;
    const closed = new Promise((resolve) => {
      finish = resolve;
    });
    const child = {
      exitCode: null,
      signalCode: null,
      kill() {
        queueMicrotask(() => {
          terminated++;
          finish();
        });
        return true;
      },
    };
    return { child, closed };
  });
  await cleanupResources(
    { directories: new Set(["fixture-lock-dir"]), children: new Set(children) },
    async () => {
      assert.equal(terminated, 2, "storage removed before every child termination was confirmed");
    },
  );
});

test("unconfirmed runtime fixture termination preserves storage and reports failure", async () => {
  let removed = false;
  const child = { exitCode: null, signalCode: null, kill: () => true };
  await assert.rejects(
    cleanupResources(
      {
        directories: new Set(["fixture-lock-dir"]),
        children: new Set([{ child, closed: new Promise(() => {}) }]),
      },
      async () => {
        removed = true;
      },
      5,
    ),
    /termination|storage/,
  );
  assert.equal(removed, false);
});

function playbackTrace(stderr) {
  if (!stderr) return [];
  assert.ok(stderr.endsWith("\n"), "Fixture traces must be complete JSON lines");
  return stderr
    .trimEnd()
    .split("\n")
    .map((line) => {
      const trace = JSON.parse(line);
      assert.deepEqual(
        Object.keys(trace)
          .filter((key) => key !== "voice")
          .sort(),
        ["fixture", "text"],
      );
      assert.equal(trace.fixture, "played");
      assert.equal(typeof trace.text, "string");
      assert.ok(trace.text.length > 0 && Buffer.byteLength(trace.text) <= 320);
      return trace;
    });
}

test(
  "stdio remains alive after more than 256 reply IDs in a bounded host burst",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    for (let i = 0; i < 300; i++)
      proc.send({ type: "reply_end", speaker: "a1", turn: 1, reply: `reply-${i}`, text: "A b c." });
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait(
      (frames) =>
        frames.some((f) => f.state === "speaking") && frames.at(-1)?.state === "listening",
    );
    assert.equal(
      proc.frames.some((f) => f.type === "error"),
      false,
    );
    proc.send({ type: "shutdown" });
    assert.equal((await proc.closed).code, 0);
  },
);

before(() => {
  executable = buildFixture(root);
});

for (const [label, name, accepted] of [
  ["exact accented UTF-8 byte boundary", "é".repeat(512), true],
  ["exact euro UTF-8 byte boundary", "€".repeat(341) + "a", true],
  ["over-budget accented device", "é".repeat(600), false],
  ["over-budget euro device", "€".repeat(600), false],
])
  test(`native stdio device validation: ${label}`, { timeout: 15_000 }, async (t) => {
    const proc = await runtime(t, ["run", "--test-fixture-loading"]);
    proc.send({ ...init, settings: { inputDevice: name, outputDevice: name } });
    if (accepted) {
      await proc.wait((frames) => frames.some((f) => f.state === "loading"));
      proc.child.stdin.end();
    }
    const result = await proc.closed;
    assert.equal(result.code, accepted ? 0 : 1);
    assert.equal(result.stderr, "");
    if (!accepted) {
      assert.ok(result.frames.some((f) => f.code === "invalid_protocol" && f.fatal));
      assert.ok(!JSON.stringify(result.frames).includes(name));
    }
  });

function storageEnvironment(inherited, key, value) {
  const env = { ...inherited };
  const controlled = new Set(["HOME", "LOCALAPPDATA", "USERPROFILE", "XDG_DATA_HOME"]);
  for (const name of Object.keys(env)) if (controlled.has(name.toUpperCase())) delete env[name];
  env[key] = value;
  // Node/libuv may restore required Windows variables omitted from an env block.
  // A nonempty invalid fallback must be explicit rather than inherited/deleted.
  if (key === "LOCALAPPDATA") env.USERPROFILE = "relative-home";
  return env;
}

test("invalid storage test environment removes all inherited case variants", () => {
  const inherited = Object.freeze({
    HOME: "inherited",
    Home: "inherited",
    home: "inherited",
    LOCALAPPDATA: "inherited",
    LocalAppData: "inherited",
    localappdata: "inherited",
    USERPROFILE: "inherited",
    UserProfile: "inherited",
    userprofile: "inherited",
    XDG_DATA_HOME: "inherited",
    Xdg_Data_Home: "inherited",
    UNCHANGED: "kept",
  });
  for (const key of ["HOME", "LOCALAPPDATA"])
    for (const value of ["", "relative-home", "."])
      assert.deepEqual(storageEnvironment(inherited, key, value), {
        UNCHANGED: "kept",
        [key]: value,
        ...(key === "LOCALAPPDATA" ? { USERPROFILE: "relative-home" } : {}),
      });
});

test("empty or relative host storage bases never create project-local locks", () => {
  const key = process.platform === "win32" ? "LOCALAPPDATA" : "HOME";
  for (const value of ["", "relative-home", "."]) {
    const env = storageEnvironment(process.env, key, value);
    const result = spawnSync(executable, ["run", "--test-fixture-loading"], {
      cwd: root,
      env,
      encoding: "utf8",
      timeout: 5000,
    });
    const diagnostic = JSON.stringify({
      key,
      value,
      status: result.status,
      signal: result.signal,
      stdout: result.stdout ?? "",
      stderr: result.stderr ?? "",
      spawnError: result.error?.message ?? null,
      ...(key === "LOCALAPPDATA"
        ? { fallbackKey: "USERPROFILE", fallbackValue: env.USERPROFILE }
        : {}),
    });
    assert.equal(result.status, 1, diagnostic);
    const frames = result.stdout
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.ok(
      frames.some(
        (f) =>
          f.code === "engine_locked" &&
          f.message.includes(key === "LOCALAPPDATA" ? "USERPROFILE" : key),
      ),
      diagnostic,
    );
    assert.equal(result.stderr, "", diagnostic);
  }
});

for (const [name, announcement, text] of [
  ["9KB announcement", true, "A short sentence. ".repeat(550)],
  ["300KB reply", false, "A short sentence. ".repeat(20_000)],
  ["70 short sentences", true, "A b c. ".repeat(70)],
])
  test(`stdio remains alive after ${name}`, { timeout: 15_000 }, async (t) => {
    const proc = await runtime(t);
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    proc.send(
      announcement
        ? { type: "announcement", speaker: "a1", reply: "long", text }
        : { type: "reply_end", speaker: "a1", turn: 1, reply: "long", text },
    );
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait(
      (frames) =>
        frames.some((f) => f.state === "speaking") && frames.at(-1)?.state === "listening",
    );
    assert.equal(
      proc.frames.some((f) => f.type === "error"),
      false,
    );
    proc.send({ type: "mute", on: true });
    await proc.wait((frames) => frames.at(-1)?.state === "muted");
    proc.send({ type: "shutdown" });
    assert.equal((await proc.closed).code, 0);
  });

async function localDir(t) {
  const base = join(tmpdir(), "opencode");
  await mkdir(base, { recursive: true });
  const dir = await mkdtemp(join(base, "converse-runtime-"));
  resources(t).directories.add(dir);
  return dir;
}

async function runtime(t, args = ["run", "--test-fixture"], dir) {
  dir ??= await localDir(t);
  const isolated =
    args.includes("--test-fixture") ||
    args.includes("--test-fixture-loading") ||
    (args[0] === "doctor" && !args.includes("--test-only-inference"));
  const command = isolated ? [...args, "--test-state-dir", dir] : args;
  const child = spawn(executable, command, {
    cwd: root,
    env: process.env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const frames = [];
  let pending = "";
  let stderr = "";
  child.stdin.on("error", () => {}); // Rejection may close stdin before the entire bad frame is written.
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (text) => {
    pending += text;
    while (pending.includes("\n")) {
      const end = pending.indexOf("\n");
      frames.push(JSON.parse(pending.slice(0, end)));
      pending = pending.slice(end + 1);
    }
  });
  child.stderr.on("data", (text) => {
    stderr += text;
  });
  const closed = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal, frames, stderr, pending }));
  });
  resources(t).children.add({ child, closed });
  async function wait(predicate) {
    const deadline = Date.now() + 5000;
    while (!predicate(frames)) {
      assert.equal(child.exitCode, null, JSON.stringify(frames));
      assert.ok(Date.now() < deadline, `Timed out waiting for runtime: ${JSON.stringify(frames)}`);
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }
  const send = (frame) => child.stdin.write(JSON.stringify(frame) + "\n");
  return { child, frames, closed, wait, send, stderr: () => stderr };
}

test(
  "stdio fixture is explicit and completes a full-reply-only turn",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    await proc.wait((frames) => frames.some((f) => f.type === "hello"));
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    const prompt = proc.frames.find((f) => f.type === "prompt");
    assert.deepEqual(prompt, {
      type: "prompt",
      turn: 1,
      speaker: "a1",
      addressed: false,
      text: "Fixture prompt.",
    });
    proc.send({
      type: "reply_end",
      speaker: "a1",
      turn: 1,
      reply: "reply-1",
      text: "This is the full reply.",
    });
    proc.send({
      type: "reply_end",
      speaker: "a1",
      turn: 1,
      reply: "reply-1",
      text: "This is the full reply.",
    });
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait((frames) => frames.at(-1)?.state === "listening");
    assert.equal(proc.frames.filter((f) => f.state === "speaking").length, 1);
    proc.send({ type: "shutdown" });
    const result = await proc.closed;
    assert.equal(result.code, 0);
    assert.deepEqual(playbackTrace(result.stderr), [
      { fixture: "played", text: "This is the full reply." },
    ]);
    assert.equal(result.pending, "");
  },
);

test(
  "mute interrupts the active turn and suppresses stale replies",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    proc.send({ type: "mute", on: true });
    proc.send({
      type: "reply_end",
      speaker: "a1",
      turn: 1,
      reply: "stale",
      text: "Do not speak this.",
    });
    proc.send({ type: "mute", on: false });
    await proc.wait((frames) => frames.at(-1)?.state === "listening");
    assert.ok(proc.frames.some((f) => f.type === "interrupt" && f.turn === 1));
    assert.equal(proc.frames.filter((f) => f.state === "speaking").length, 0);
    proc.child.stdin.end();
    assert.equal((await proc.closed).code, 0);
  },
);

test(
  "pause holds spoken output and resumes it without a restart",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    proc.send({ type: "pause", on: true });
    proc.send({
      type: "reply_end",
      speaker: "a1",
      turn: 1,
      reply: "held",
      text: "Wait for resume.",
    });
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait((frames) => frames.some((f) => f.state === "speaking"));
    // Give the fixture several playback ticks; nothing may reach the device trace.
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(playbackTrace(proc.stderr()), []);
    assert.equal(proc.child.exitCode, null, "pausing must not end the runtime");

    proc.send({ type: "pause", on: false });
    await proc.wait((frames) => frames.at(-1)?.state === "listening");
    assert.deepEqual(playbackTrace(proc.stderr()), [
      { fixture: "played", text: "Wait for resume." },
    ]);
    proc.send({ type: "shutdown" });
    const result = await proc.closed;
    assert.equal(result.code, 0);
    assert.deepEqual(playbackTrace(result.stderr), [
      { fixture: "played", text: "Wait for resume." },
    ]);
  },
);

test(
  "stdio fixture plays a fragmented decimal as one intact speech chunk",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    proc.send({
      type: "reply_delta",
      speaker: "a1",
      turn: 1,
      reply: "decimal",
      text: "The value is 3.",
    });
    proc.send({ type: "reply_delta", speaker: "a1", turn: 1, reply: "decimal", text: "14." });
    proc.send({
      type: "reply_end",
      speaker: "a1",
      turn: 1,
      reply: "decimal",
      text: "The value is 3.14.",
    });
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait(
      (frames) =>
        frames.some((f) => f.state === "speaking") && frames.at(-1)?.state === "listening",
    );
    proc.send({ type: "shutdown" });
    const result = await proc.closed;
    assert.equal(result.code, 0);
    assert.deepEqual(playbackTrace(result.stderr), [
      { fixture: "played", text: "The value is 3.14." },
    ]);
  },
);

test(
  "explicit fixture traces only sanitized completed playback once",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    proc.send(init);
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait((frames) => frames.at(-1)?.state === "listening");
    const announcement = {
      type: "announcement",
      speaker: "a1",
      reply: "worker-update",
      text: "Worker **finished** safely.\n```\nsecret();\n```\n",
    };
    proc.send(announcement);
    await proc.wait(
      (frames) =>
        frames.some((f) => f.state === "speaking") && frames.at(-1)?.state === "listening",
    );
    proc.send(announcement);
    proc.send({ type: "shutdown" });
    const result = await proc.closed;
    assert.equal(result.code, 0);
    assert.deepEqual(playbackTrace(result.stderr), [
      { fixture: "played", text: "Worker finished safely." },
    ]);
  },
);

for (const [name, input] of [
  ["unknown message", '{"type":"unknown"}\n'],
  ["unknown fields", '{"type":"init","protocol":3,"settings":{"modelPath":"/untrusted"}}\n'],
  ["unit-message unknown fields", '{"type":"shutdown","surprise":true}\n'],
  ["protocol mismatch", '{"type":"init","protocol":1,"settings":{}}\n'],
  ["malformed JSON", "{definitely not JSON}\n"],
  ["oversized unterminated frame", "x".repeat(1_048_577)],
  ["oversized terminated frame", "x".repeat(1_048_577) + "\n"],
])
  test(`stdio rejects ${name} without reflecting input`, { timeout: 15_000 }, async (t) => {
    const proc = await runtime(t);
    proc.child.stdin.write(input);
    const result = await proc.closed;
    assert.equal(result.code, 1);
    assert.ok(result.frames.some((f) => f.type === "error" && f.fatal));
    assert.equal(result.stderr, "");
    assert.ok(!JSON.stringify(result.frames).includes("/untrusted"));
  });

test("EOF shuts down while loading, without a ready event", { timeout: 10_000 }, async (t) => {
  const proc = await runtime(t, ["run", "--test-fixture-loading"]);
  proc.send(init);
  await proc.wait((frames) => frames.some((f) => f.state === "loading"));
  proc.child.stdin.end();
  const result = await proc.closed;
  assert.equal(result.code, 0);
  assert.equal(result.stderr, "");
  assert.equal(
    result.frames.some((f) => f.state === "listening"),
    false,
  );
});

test(
  "OS lock rejects concurrent runtimes and releases after EOF",
  { timeout: 15_000 },
  async (t) => {
    const dir = await localDir(t);
    const first = await runtime(t, ["run", "--test-fixture-loading"], dir);
    first.send(init);
    await first.wait((frames) => frames.some((f) => f.state === "loading"));
    const second = await runtime(t, ["run", "--test-fixture-loading"], dir);
    const denied = await second.closed;
    assert.equal(denied.code, 1);
    assert.ok(denied.frames.some((f) => f.code === "engine_locked"));
    first.child.stdin.end();
    assert.equal((await first.closed).code, 0);
    const third = await runtime(t, ["run", "--test-fixture-loading"], dir);
    third.send(init);
    await third.wait((frames) => frames.some((f) => f.state === "loading"));
    third.child.stdin.end();
    assert.equal((await third.closed).code, 0);
  },
);

test("no-model doctor is actionable and does not open audio", { timeout: 10_000 }, async (t) => {
  const proc = await runtime(t, ["doctor"]);
  const result = await proc.closed;
  assert.equal(result.code, 1);
  assert.deepEqual(result.frames, []);
  assert.match(result.stderr, /models install/);
  assert.match(result.stderr, /does not open devices/);
  assert.match(result.stderr, /Native audio compiled: false/);
});

test("normal run never silently starts fixture mode", { timeout: 10_000 }, async (t) => {
  const proc = await runtime(t, ["run"]);
  proc.send(init);
  const result = await proc.closed;
  assert.equal(result.code, 1);
  assert.equal(result.stderr, "");
  assert.equal(
    result.frames.some((f) => f.type === "prompt"),
    false,
  );
  assert.ok(result.frames.some((f) => f.code === "native_unavailable"));
});

const roster = [
  { id: "a1", name: "Ada", voice: "zoey" },
  { id: "a2", name: "Grace", voice: "easton" },
];

test(
  "stdio fixture speaks each participant in its own voice and shows who is thinking or speaking",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    await proc.wait((frames) => frames.some((f) => f.type === "hello"));
    proc.send({ ...init, settings: { participants: roster } });
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    const frames = proc.frames;
    assert.equal(frames.find((f) => f.type === "prompt").speaker, "a1");
    assert.deepEqual(
      frames.find((f) => f.type === "status" && f.state === "thinking"),
      {
        type: "status",
        state: "thinking",
        speaker: "a1",
      },
    );
    proc.send({ type: "reply_end", speaker: "a1", turn: 1, reply: "r1", text: "Ada speaks." });
    proc.send({ type: "reply_end", speaker: "a2", turn: 1, reply: "r2", text: "Grace waits." });
    proc.send({ type: "turn_end", turn: 1, reason: "completed" });
    await proc.wait((all) => {
      const speaking = all.findLastIndex((f) => f.type === "status" && f.speaker === "a2");
      return speaking >= 0 && all.slice(speaking).some((f) => f.state === "listening");
    });
    proc.send({ type: "shutdown" });
    const result = await proc.closed;
    assert.equal(result.code, 0);
    assert.deepEqual(playbackTrace(result.stderr), [
      { fixture: "played", text: "Ada speaks.", voice: "zoey" },
      { fixture: "played", text: "Grace waits.", voice: "easton" },
    ]);
    assert.ok(
      result.frames.some(
        (f) => f.type === "status" && f.state === "speaking" && f.speaker === "a1",
      ),
    );
    assert.ok(
      result.frames.some(
        (f) => f.type === "status" && f.state === "speaking" && f.speaker === "a2",
      ),
    );
  },
);

test("stdio rejects an invalid roster and speakers outside it", { timeout: 15_000 }, async (t) => {
  for (const settings of [
    { participants: [] },
    { participants: [{ id: "a1", name: "Ada", voice: "nobody" }] },
    {
      participants: [
        { id: "a1", name: "Ada", voice: "zoey" },
        { id: "a1", name: "Bo", voice: "maeve" },
      ],
    },
  ]) {
    const proc = await runtime(t);
    await proc.wait((frames) => frames.some((f) => f.type === "hello"));
    proc.send({ ...init, settings });
    const result = await proc.closed;
    assert.ok(
      result.frames.some((f) => f.code === "invalid_protocol" && f.fatal),
      JSON.stringify(settings),
    );
  }
  const proc = await runtime(t);
  await proc.wait((frames) => frames.some((f) => f.type === "hello"));
  proc.send({ ...init, settings: { participants: roster } });
  await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
  proc.send({ type: "reply_end", speaker: "a9", turn: 1, reply: "r", text: "Nobody." });
  const result = await proc.closed;
  assert.ok(result.frames.some((f) => f.code === "invalid_state" && f.fatal));
});

const reply = (proc, turn, speaker, text) => {
  proc.send({ type: "reply_end", speaker, turn, reply: `r${turn}`, text });
  proc.send({ type: "turn_end", turn, reason: "completed" });
};

test(
  "stdio relays a reply addressed to another agent as a new prompt and speaks both voices",
  { timeout: 15_000 },
  async (t) => {
    const proc = await runtime(t);
    await proc.wait((frames) => frames.some((f) => f.type === "hello"));
    proc.send({ ...init, settings: { participants: roster } });
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    reply(proc, 1, "a1", "Grace, can you check the build?");
    await proc.wait((frames) => frames.some((f) => f.type === "prompt" && f.from));
    assert.deepEqual(
      proc.frames.find((f) => f.type === "prompt" && f.from),
      {
        type: "prompt",
        turn: 2,
        speaker: "a2",
        addressed: true,
        text: "can you check the build?",
        from: "a1",
      },
    );
    reply(proc, 2, "a2", "It is green.");
    await proc.wait((all) => {
      const last = all.findLastIndex((f) => f.type === "status" && f.speaker === "a2");
      return last >= 0 && all.slice(last).some((f) => f.state === "listening");
    });
    proc.send({ type: "shutdown" });
    const result = await proc.closed;
    assert.equal(result.code, 0);
    assert.deepEqual(playbackTrace(result.stderr), [
      { fixture: "played", text: "Grace, can you check the build?", voice: "zoey" },
      { fixture: "played", text: "It is green.", voice: "easton" },
    ]);
  },
);

test(
  "stdio ends a runaway chain at four agent turns with a reason, and never relays with chat off",
  { timeout: 20_000 },
  async (t) => {
    const proc = await runtime(t);
    await proc.wait((frames) => frames.some((f) => f.type === "hello"));
    proc.send({ ...init, settings: { participants: roster } });
    await proc.wait((frames) => frames.some((f) => f.type === "prompt"));
    for (let turn = 1; turn <= 4; turn++) {
      reply(proc, turn, turn % 2 ? "a1" : "a2", turn % 2 ? "Grace, you." : "Ada, you.");
      if (turn < 4)
        await proc.wait((frames) => frames.some((f) => f.type === "prompt" && f.turn === turn + 1));
    }
    await proc.wait((frames) => frames.some((f) => f.type === "chain_end"));
    assert.deepEqual(
      proc.frames.find((f) => f.type === "chain_end"),
      { type: "chain_end", turn: 4, reason: "turns" },
    );
    assert.equal(proc.frames.filter((f) => f.type === "prompt").length, 4);
    proc.send({ type: "shutdown" });
    assert.equal((await proc.closed).code, 0);

    const off = await runtime(t);
    await off.wait((frames) => frames.some((f) => f.type === "hello"));
    off.send({ ...init, settings: { participants: roster, agentChat: "off" } });
    await off.wait((frames) => frames.some((f) => f.type === "prompt"));
    reply(off, 1, "a1", "Grace, you.");
    await off.wait((all) => all.some((f) => f.type === "status" && f.state === "listening"));
    off.send({ type: "shutdown" });
    const result = await off.closed;
    assert.equal(result.frames.filter((f) => f.type === "prompt").length, 1);
    assert.ok(!result.frames.some((f) => f.type === "chain_end"));
  },
);
