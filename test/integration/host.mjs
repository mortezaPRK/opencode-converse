// Opt in explicitly: npm run build && node --test test/integration/host.mjs
// Requires an installed OpenCode 2.3 executable (OPENCODE_HOST_BINARY overrides PATH).
// This is host integration with a scripted transport, NOT an AI/audio/device test.
// V2 contracts: https://opencode.ai/docs/build/plugins/server, /config, /providers,
// /models, /api; input shapes are from installed @opencode/{client,plugin} 2.3.0.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { once } from "node:events";
import { access, chmod, mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { OpenCode } from "@opencode/client";
import { Rpc } from "@opencode/plugin/rpc";
import { Converse } from "../../dist/rpc.js";
import { hostTransports } from "../client-host-transport.mjs";
import { removeAfterClose } from "../client-process-cleanup.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));
const model = { providerID: "host-fixture", id: "scripted" };
const names = [
  "list_projects",
  "list_sessions",
  "session_status",
  "start_session",
  "send_message",
  "interrupt_session",
].map((name) => `converse_${name}`);
const passthrough = {
  "~standard": { version: 1, vendor: "host-fixture", validate: (value) => ({ value }) },
};
const method = { input: passthrough, output: passthrough };
const observer = Rpc.define({
  id: "converse-host-observer",
  methods: { inspect: method, invoke: method, permission: method },
  events: {},
});

async function executable() {
  const requested = process.env.OPENCODE_HOST_BINARY ?? "opencode";
  const candidates = isAbsolute(requested)
    ? [requested]
    : (process.env.PATH ?? "").split(delimiter).map((dir) => resolve(dir, requested));
  for (const candidate of candidates) {
    try {
      await access(candidate, 1);
      return candidate;
    } catch {
      // An explicitly invoked integration must fail, never silently skip.
    }
  }
  throw new Error(`OpenCode executable unavailable: ${requested}; install OpenCode 2.3 first`);
}

async function bounded(promise, description, milliseconds = 10_000) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`Timed out: ${description}`)), milliseconds);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function hostTest(t) {
  const binary = await executable();
  await access(join(root, "dist/index.js"));
  const temporaryBase = join(tmpdir(), "opencode");
  await mkdir(temporaryBase, { recursive: true, mode: 0o700 });
  const temporary = await mkdtemp(join(temporaryBase, "converse-host-"));
  await chmod(temporary, 0o700);
  const directory = join(temporary, "project");
  const home = join(temporary, "home");
  const fixture = join(temporary, "observer");
  const requests = [];
  const transportErrors = [];
  let nextCall;
  let child;
  let closed;
  let childError;
  let output = "";
  let eventController;
  let eventTask;
  const password = randomBytes(24).toString("hex");
  const llm = createServer(async (req, res) => {
    try {
      assert.equal(req.method, "POST");
      assert.equal(req.url, "/v1/chat/completions", "No proxy forwarding or discovery allowed");
      let body = "";
      for await (const chunk of req) {
        body += chunk;
        assert.ok(body.length < 1_000_000, "Bounded fixture request body");
      }
      const input = JSON.parse(body);
      assert.equal(input.model, model.id);
      assert.ok(requests.length < 30, "No unbounded generation loop");
      requests.push(input);
      const call = nextCall;
      nextCall = undefined;
      const message = call
        ? {
            role: "assistant",
            content: null,
            tool_calls: [
              {
                id: `call_fixture_${requests.length}`,
                type: "function",
                function: { name: call.name, arguments: JSON.stringify(call.input) },
              },
            ],
          }
        : { role: "assistant", content: "Fixture completed." };
      const finish = call ? "tool_calls" : "stop";
      if (input.stream) {
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        const delta = call
          ? {
              role: "assistant",
              tool_calls: [{ index: 0, ...message.tool_calls[0] }],
            }
          : message;
        const chunk = (value, reason) => ({
          id: `chatcmpl_fixture_${requests.length}`,
          object: "chat.completion.chunk",
          created: 1,
          model: model.id,
          choices: [{ index: 0, delta: value, finish_reason: reason }],
        });
        res.write(`data: ${JSON.stringify(chunk(delta, null))}\n\n`);
        res.write(`data: ${JSON.stringify(chunk({}, finish))}\n\n`);
        res.end("data: [DONE]\n\n");
      } else {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            id: "chatcmpl_fixture",
            object: "chat.completion",
            created: 1,
            model: model.id,
            choices: [{ index: 0, message, finish_reason: finish }],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          }),
        );
      }
    } catch (error) {
      transportErrors.push(error.message);
      res.writeHead(400).end("Fixture rejected unexpected request");
    }
  });
  llm.requestTimeout = 5000;
  llm.on("connect", (_req, socket) => {
    transportErrors.push("External HTTPS proxy request rejected");
    socket.destroy();
  });
  t.after(async () => {
    try {
      eventController?.abort();
      if (child && child.exitCode === null && child.signalCode === null) {
        child.stdin.end();
        child.kill("SIGTERM");
        try {
          await bounded(closed, "owned OpenCode shutdown", 5000);
        } catch {
          child.kill("SIGKILL");
          await bounded(closed, "owned OpenCode forced shutdown", 3000);
        }
      }
      if (eventTask) await bounded(eventTask, "actual host event stream cleanup", 3000);
    } finally {
      llm.closeAllConnections();
      try {
        await bounded(new Promise((done) => llm.close(done)), "fixture HTTP shutdown", 2000);
      } finally {
        await removeAfterClose(temporary, { child, closed });
      }
    }
  });
  await Promise.all([directory, home, fixture].map((dir) => mkdir(dir)));
  llm.listen(0, "127.0.0.1");
  await once(llm, "listening");
  const localURL = `http://127.0.0.1:${llm.address().port}`;
  // This observer does not register/replace tools, agents, hooks, or model output.
  // Direct invocation deliberately bypasses model visibility to test Converse's
  // own authorization guard, not the host dispatcher's permission enforcement.
  await writeFile(
    join(fixture, "package.json"),
    JSON.stringify({
      name: "converse-host-observer",
      type: "module",
      exports: { ".": "./index.js" },
    }),
  );
  await writeFile(
    join(fixture, "index.js"),
    `
import { Plugin } from "@opencode/plugin";
import { Rpc } from "@opencode/plugin/rpc";
const schema = { "~standard": { version: 1, vendor: "host-fixture", validate: value => ({ value }) } };
const method = { input: schema, output: schema };
const contract = Rpc.define({ id: "converse-host-observer", methods: {
  inspect: method, invoke: method, permission: method
}, events: {} });
export default Plugin.define({ id: contract.id, async setup(ctx) {
  const original = globalThis.fetch;
  globalThis.fetch = (input, init) => {
    const url = new URL(typeof input === "string" || input instanceof URL ? input : input.url);
    if (url.hostname !== "127.0.0.1" && url.hostname !== "localhost")
      throw new Error("Host fixture forbids external fetch: " + url.origin);
    return original(input, init);
  };
  const registration = await ctx.rpc.register(contract, {
    inspect: async () => (await ctx.tool.list()).filter(t => t.id.startsWith("converse_")).map(t => ({
      id: t.id, options: t.options
    })),
    invoke: async ({ name, input, sessionID }) => {
      if (!name.startsWith("converse_")) throw new Error("Only Converse executors may be probed");
      const tool = (await ctx.tool.list()).find(t => t.id === name);
      if (!tool) throw new Error("Missing registered tool: " + name);
      try { return { result: await tool.execute(input, { sessionID, signal: AbortSignal.timeout(5000) }) }; }
      catch (error) { return { error: error.message }; }
    },
    permission: async ({ sessionID, action }) => {
      try {
        await ctx.permission.assert({ sessionID, action, resources: ["fixture"], save: [] });
        return { allowed: true };
      } catch (error) { return { allowed: false, error: error.message }; }
    }
  });
  return async () => { await registration.dispose(); globalThis.fetch = original; };
} });
`,
  );
  const config = {
    $schema: "https://opencode.ai/config.json",
    update: "disable",
    snapshots: false,
    model: `${model.providerID}/${model.id}`,
    default_agent: "build",
    plugins: [
      fixture,
      {
        package: root,
        options: {
          supervisorModel: `${model.providerID}/${model.id}`,
          workerModel: `${model.providerID}/${model.id}`,
        },
      },
      "-opencode.provider.ollama",
      "-opencode.provider.lmstudio",
      "-opencode.provider.vllm",
    ],
    policies: [
      { action: "provider.use", resource: "*", effect: "deny" },
      { action: "provider.use", resource: model.providerID, effect: "allow" },
    ],
    providers: {
      [model.providerID]: {
        name: "Scripted localhost fixture",
        package: "@opencode/ai/providers/openai-compatible",
        settings: { baseURL: `${localURL}/v1`, apiKey: "throwaway-fixture", timeout: 5000 },
        models: {
          [model.id]: {
            capabilities: { tools: true, input: ["text"], output: ["text"] },
            limit: { context: 32768, output: 1024 },
          },
        },
      },
    },
  };
  const configHome = join(home, "config");
  await mkdir(join(configHome, "opencode"), { recursive: true });
  await writeFile(join(configHome, "opencode/opencode.json"), JSON.stringify(config));
  // Allowlist environment: never inherit credentials, user data, or OpenCode config.
  child = spawn(
    binary,
    ["serve", "--hostname", "127.0.0.1", "--port", "0", "--log-level", "debug", "--print-logs"],
    {
      cwd: directory,
      stdio: "pipe",
      env: {
        PATH: `${dirname(binary)}:/usr/bin:/bin`,
        HOME: home,
        TMPDIR: temporary,
        XDG_CONFIG_HOME: configHome,
        XDG_DATA_HOME: join(home, "data"),
        XDG_CACHE_HOME: join(home, "cache"),
        XDG_STATE_HOME: join(home, "state"),
        OPENCODE_DB: ":memory:",
        OPENCODE_DISABLE_MODELS_FETCH: "1",
        OPENCODE_DISABLE_FREE_MODELS: "1",
        OPENCODE_DISABLE_AUTOUPDATE: "1",
        OPENCODE_CONFIG_PROJECT_DISABLE: "1",
        OPENCODE_SERVER_PASSWORD: password,
        HTTP_PROXY: localURL,
        HTTPS_PROXY: localURL,
        NO_PROXY: "127.0.0.1,localhost",
      },
    },
  );
  closed = new Promise((done) => child.once("close", done));
  child.on("error", (error) => {
    childError = error;
  });
  const baseUrl = await bounded(
    new Promise((done, reject) => {
      const consume = (chunk) => {
        output = (output + chunk).slice(-16_000);
        const match = output.match(/http:\/\/127\.0\.0\.1:\d+/);
        if (match) done(match[0]);
      };
      child.stdout.on("data", consume);
      child.stderr.on("data", consume);
      child.once("error", reject);
      child.once("close", () => reject(new Error(`OpenCode exited during startup: ${output}`)));
    }),
    "private host listener",
    15_000,
  );
  const transport = hostTransports(baseUrl);
  const connection = {
    baseUrl,
    headers: { Authorization: `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}` },
  };
  const client = OpenCode.make({ ...connection, fetch: transport.request });
  const streamClient = OpenCode.make({ ...connection, fetch: transport.subscription });
  const location = { directory };
  const rpc = client.rpc(Converse);
  const observe = client.rpc(observer);
  const options = () => ({ location, signal: AbortSignal.timeout(10_000) });
  const readiness = AbortSignal.timeout(15_000);
  await bounded(
    (async () => {
      while (true) {
        readiness.throwIfAborted();
        if (childError) throw childError;
        const page = await client.plugin.list({ location }, { signal: readiness });
        const helper = page.data.find((entry) => entry.id === observer.id);
        const failed = page.data.find((entry) => entry.state.status === "failed");
        if (failed) throw new Error(`Plugin ${JSON.stringify(failed)}: ${output}`);
        const plugin = page.data.find((entry) => entry.id === "converse");
        if (plugin?.state.status === "failed") throw new Error(plugin.state.error);
        if (
          plugin?.state.status === "active" &&
          plugin.features.rpc &&
          helper?.state.status === "active"
        )
          break;
        await delay(100, undefined, { signal: readiness });
      }
    })(),
    "lazy Converse activation",
    15_000,
  );
  const hello = await rpc.hello(undefined, options());
  assert.equal(hello.protocol, 2);
  assert.ok(hello.packageRoot, "The server must report its package root for the local GUI engine");
  const info = await client.server.info();
  assert.match(info.version, /^2\.3\./, "This regression targets the installed 2.3 host contract");
  const agents = await client.agent.list({ location });
  const agent = agents.data.find((entry) => entry.id === "converse");
  assert.ok(agent, "Converse must actually be registered by its transform");
  assert.equal(agent.hidden, true);
  assert.equal(agent.mode, "primary");
  assert.deepEqual(agent.permissions.slice(0, 2), [
    { action: "*", resource: "*", effect: "deny" },
    { action: "converse", resource: "*", effect: "allow" },
  ]);
  assert.ok(
    agent.permissions.slice(2).every((rule) => rule.effect === "deny"),
    "Host-added capability rules must not broaden supervisor access",
  );
  const workerAgent = agents.data.find((entry) => entry.id === "converse-worker");
  assert.ok(workerAgent, "The dedicated worker agent must be registered");
  assert.equal(workerAgent.hidden, true);
  assert.equal(workerAgent.mode, "primary");
  const workerProbe = await client.session.create({
    title: "Worker agent probe",
    agent: "converse-worker",
    model,
    location,
  });
  assert.equal(workerProbe.agent, "converse-worker");
  const tools = await observe.inspect(undefined, options()).catch((error) => {
    throw new Error(`Observer inspect: ${JSON.stringify(error)}\n${output}`);
  });
  assert.deepEqual(tools.map((tool) => tool.id).sort(), [...names].sort());
  for (const tool of tools) {
    assert.equal(tool.options.namespace, "converse");
    assert.equal(tool.options.codemode, false);
    assert.equal(tool.options.permission, "converse");
  }
  const { sessionID, runtime } = await rpc.start(undefined, options());
  assert.deepEqual(runtime, {});
  const owned = await client.session.get({ sessionID });
  t.diagnostic(
    `Literal location guard: requested=${JSON.stringify(directory)}; reported=${JSON.stringify(owned.location.directory)}; matchingEvents=0 (before owned turn)`,
  );
  assert.equal(owned.location.directory, directory);
  const ordinary = await client.session.create({
    title: "Ordinary fixture",
    agent: "build",
    model,
    location,
  });
  const forged = await client.session.create({
    title: "Forged Converse",
    agent: "converse",
    model,
    location,
    metadata: owned.metadata,
  });
  const wrongAgent = await rpc.start(undefined, options());
  await client.session.switchAgent({ sessionID: wrongAgent.sessionID, agent: "build" });
  for (const invalid of [ordinary.id, forged.id, wrongAgent.sessionID]) {
    for (const name of names) {
      const result = await observe.invoke({ name, input: {}, sessionID: invalid }, options());
      assert.match(result.error, /requires its owning Converse supervisor/);
    }
  }
  const allowed = await observe.invoke(
    { name: "converse_list_projects", input: {}, sessionID },
    options(),
  );
  assert.ok(allowed.result, "The actual executor must work for an owned supervisor");
  const denied = await observe.permission({ sessionID, action: "shell" }, options());
  assert.equal(denied.allowed, false, "Host permission evaluation denies supervisor shell access");
  assert.deepEqual(
    await client.permission.list({ sessionID }),
    [],
    "Deny must not ask for approval",
  );
  async function turn(id, label, call) {
    const before = requests.length;
    nextCall = call;
    await client.session.prompt({ sessionID: id, text: label });
    await client.session.wait({ sessionID: id });
    assert.deepEqual(transportErrors, []);
    const captured = requests
      .slice(before)
      .filter((request) =>
        request.messages.some((message) => JSON.stringify(message.content).includes(label)),
      );
    assert.ok(captured.length, `Real context dispatch reached localhost for ${label}`);
    return captured;
  }
  eventController = new AbortController();
  let matchingEvents = 0;
  let eventError;
  let connected;
  let observed;
  const eventReady = new Promise((done) => {
    connected = done;
  });
  const eventObserved = new Promise((done) => {
    observed = done;
  });
  eventTask = (async () => {
    try {
      for await (const event of streamClient.event.subscribe({ signal: eventController.signal })) {
        if (event.type === "server.connected") connected();
        if (event.data?.sessionID !== sessionID) continue;
        matchingEvents++;
        assert.ok(
          event.location?.directory === undefined || event.location.directory === directory,
          `Literal event location mismatch: requested=${JSON.stringify(directory)}; reported=${JSON.stringify(event.location?.directory)}; event=${event.type}`,
        );
        observed();
      }
      if (!eventController.signal.aborted) throw new Error("Actual host event stream ended early");
    } catch (error) {
      if (!eventController.signal.aborted) eventError = error;
    } finally {
      connected();
      observed();
    }
  })();
  await bounded(eventReady, "actual server.connected before owned turn", 5000);
  if (eventError) throw eventError;
  const supervised = await turn(sessionID, "owned-context-fixture", {
    name: "converse_list_projects",
    input: {},
  });
  await bounded(eventObserved, "at least one actual owned-session event", 5000);
  if (eventError) throw eventError;
  assert.ok(matchingEvents > 0, "Literal event location guard must not pass vacuously");
  for (const request of supervised) {
    assert.deepEqual(
      (request.tools ?? []).map((tool) => tool.function.name).sort(),
      [...names].sort(),
    );
  }
  const context = await client.session.context({ sessionID });
  assert.ok(
    context.some(
      (message) =>
        message.type === "assistant" &&
        message.content.some(
          (part) =>
            part.type === "tool" &&
            part.name === "converse_list_projects" &&
            part.state.status === "completed",
        ),
    ),
    "Host actually executed the scripted Converse call",
  );
  for (const [id, label] of [
    [ordinary.id, "ordinary-context-fixture"],
    [forged.id, "forged-context-fixture"],
  ]) {
    const captured = await turn(id, label);
    for (const request of captured) {
      const outgoing = (request.tools ?? []).map((tool) => tool.function.name);
      assert.ok(!outgoing.some((name) => name.startsWith("converse_")));
      if (id === forged.id)
        assert.deepEqual(outgoing, [], "Nonowned Converse has no supervisor or coding tools");
      else assert.ok(outgoing.length, "Ordinary coding session retains coding tools");
    }
  }
  // Owned caller still cannot inspect/control a worker belonging to somebody else.
  for (const name of [
    "converse_session_status",
    "converse_send_message",
    "converse_interrupt_session",
  ]) {
    const result = await observe.invoke(
      {
        name,
        input: {
          sessionID: ordinary.id,
          ...(name === "converse_send_message" ? { text: "must not be admitted" } : {}),
        },
        sessionID,
      },
      options(),
    );
    assert.match(result.error, /not a worker owned by this conversation/);
  }
  await rpc.stop({ sessionID }, options());
  const stopped = await observe.invoke(
    { name: "converse_list_projects", input: {}, sessionID },
    options(),
  );
  assert.match(stopped.error, /requires its owning Converse supervisor/);
  eventController.abort();
  await bounded(eventTask, "actual host event stream completion", 3000);
  if (eventError) throw eventError;
  t.diagnostic(
    `Literal location guard: requested=${JSON.stringify(directory)}; reported=${JSON.stringify(owned.location.directory)}; matchingEvents=${matchingEvents}`,
  );
  t.diagnostic(
    `OpenCode ${info.version}; ${requests.length} scripted localhost requests; no cloud model, downloads, microphone or Rust runtime`,
  );
}

test(
  "real OpenCode host: supervisor registration, context isolation and executor guards",
  { timeout: 90_000 },
  hostTest,
);
