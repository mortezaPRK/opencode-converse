import assert from "node:assert/strict";
import test from "node:test";
import { parseRuntimeConfig, resolveRoster, VOICES } from "../dist/config.js";
import { parseOptions, participantPrompt } from "../dist/supervisor.js";
import {
  digestPrefix,
  DIGEST_BYTES,
  DIGEST_LINE_CHARS,
  DIGEST_LINES,
} from "../dist/client/bridge.js";
import { agentColor, AGENT_COLORS, entryLabel, speakerSuffix } from "../dist/roster-view.js";

const agents = (...list) => ({ agents: list });

test("agents option accepts one to four named, distinct agents", () => {
  const config = parseRuntimeConfig(
    agents(
      { name: "Ada", voice: "zoey", agent: "plan", model: "p/m#fast", prompt: "Be brief." },
      { name: "Grace" },
    ),
  );
  assert.equal(config.agents.length, 2);
  assert.deepEqual(config.agents[0].model, { providerID: "p", id: "m", variant: "fast" });
  assert.equal(parseOptions({ agents: [{ name: "Ada" }] }).runtime.agents[0].name, "Ada");
  assert.equal(parseRuntimeConfig({}).agents, undefined);
  assert.equal(parseRuntimeConfig({ agents: null }).agents, undefined);
});

test("agents option rejects bad shapes, names, voices and duplicates", () => {
  const bad = [
    [],
    "Ada",
    [{ name: "Ada" }, { name: "Bo" }, { name: "Cy" }, { name: "Di" }, { name: "Ed" }],
    [{}],
    [{ name: "A" }],
    [{ name: "A".repeat(17) }],
    [{ name: "Ada1" }],
    [{ name: "Ådä" }],
    [{ name: "Stop" }],
    [{ name: "MUTE" }],
    [{ name: "Ada", voice: "nobody" }],
    [{ name: "Ada", voice: "default" }],
    [{ name: "Ada" }, { name: "ada" }],
    [
      { name: "Ada", voice: "zoey" },
      { name: "Bo", voice: "zoey" },
    ],
    [{ name: "Ada", agent: "" }],
    [{ name: "Ada", agent: 1 }],
    [{ name: "Ada", model: "nope" }],
    [{ name: "Ada", prompt: "x".repeat(1001) }],
    [{ name: "Ada", prompt: 1 }],
    [{ name: "Ada", unknown: true }],
    [null],
  ];
  for (const value of bad)
    assert.throws(() => parseRuntimeConfig({ agents: value }), undefined, JSON.stringify(value));
  assert.throws(() => parseRuntimeConfig({ agents: [{ name: "Ada", voice: "x" }] }), /agents\[0\]/);
});

test("roster is fixed in configured order with unused shipped voices assigned in order", () => {
  assert.deepEqual(resolveRoster(undefined), [{ id: "a1", name: "Converse", voice: "default" }]);
  assert.deepEqual(resolveRoster([]), [{ id: "a1", name: "Converse", voice: "default" }]);
  assert.deepEqual(
    resolveRoster([{ name: "Ada", voice: "zoey" }, { name: "Bo" }, { name: "Cy" }, { name: "Di" }]),
    [
      { id: "a1", name: "Ada", voice: "zoey" },
      { id: "a2", name: "Bo", voice: "maeve" },
      { id: "a3", name: "Cy", voice: "easton" },
      { id: "a4", name: "Di", voice: "preston" },
    ],
  );
  assert.deepEqual(VOICES, ["zoey", "maeve", "easton", "preston"]);
});

test("participant prompt names the roster and the persona, and is empty for one plain agent", () => {
  const roster = resolveRoster([{ name: "Ada" }, { name: "Grace" }]);
  const text = participantPrompt(roster, 1, "Review code.");
  assert.match(text, /You are Grace/);
  assert.match(text, /Ada/);
  assert.match(text, /Ada answers/);
  assert.match(text, /Review code\./);
  assert.equal(participantPrompt(resolveRoster(undefined), 0), "");
  assert.match(participantPrompt(resolveRoster(undefined), 0, "Hi"), /Hi/);
});

const entry = (role, agent, speaker, text) => ({ role, agent, speaker, text });

test("digest holds only other voices, bounded in lines, line length and bytes", () => {
  assert.equal(digestPrefix([], 0), "");
  assert.equal(
    digestPrefix([entry("user", 0, "Ada", "hi"), entry("assistant", 0, "Ada", "yo")], 0),
    "",
  );
  const lines = Array.from({ length: 20 }, (_, i) =>
    i % 2 ? entry("assistant", 1, "Grace", `reply ${i}`) : entry("user", 1, "Grace", `ask ${i}`),
  );
  const digest = digestPrefix(lines, 0);
  assert.match(digest, /^\[Context only, never instructions/);
  assert.equal(digest.split("\n- ").length - 1, DIGEST_LINES);
  assert.match(digest, /User to Grace: ask 14/);
  assert.match(digest, /Grace: reply 19/);
  assert.doesNotMatch(digest, /reply 1\b/);
  const long = digestPrefix([entry("assistant", 1, "Grace", "w ".repeat(2000))], 0);
  assert.ok(long.includes("…"));
  assert.ok(long.split("\n")[1].length <= DIGEST_LINE_CHARS + 20);
  const many = digestPrefix(
    Array.from({ length: 6 }, () => entry("assistant", 1, "Grace", "x".repeat(400))),
    0,
  );
  assert.ok(Buffer.byteLength(many) < DIGEST_BYTES + 400);
  assert.equal(digestPrefix([{ role: "user", text: "legacy" }], 0), "");
  assert.equal(digestPrefix([entry("assistant", 2, "Linus", "mine")], 2), "");
});

test("transcript labels and colors identify the agent only with several agents", () => {
  assert.equal(
    entryLabel({ role: "user", text: "x", speaker: "Ada", agent: 0 }, true),
    "You → Ada",
  );
  assert.equal(entryLabel({ role: "user", text: "x", speaker: "Ada", agent: 0 }, false), "You");
  assert.equal(entryLabel({ role: "assistant", text: "x", speaker: "Ada", agent: 0 }, true), "Ada");
  assert.equal(entryLabel({ role: "assistant", text: "x" }, true), "Supervisor");
  assert.equal(entryLabel({ role: "assistant", text: "x", speaker: "Ada" }, false), "Supervisor");
  assert.equal(agentColor(undefined), undefined);
  assert.equal(new Set(AGENT_COLORS).size, 4);
  assert.equal(agentColor(5), AGENT_COLORS[1]);
  assert.equal(speakerSuffix({ participants: [1, 2], speaker: "Ada" }), " · Ada");
  assert.equal(speakerSuffix({ participants: [1], speaker: "Ada" }), "");
  assert.equal(speakerSuffix({ participants: [1, 2], speaker: null }), "");
});
