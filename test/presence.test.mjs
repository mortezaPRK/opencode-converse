import assert from "node:assert/strict";
import test from "node:test";
import {
  normalizeVoiceStatus,
  presence,
  presenceGlyph,
  PRESENCE_ERROR_MS,
} from "../dist/presence.js";

const states = ["off", "loading", "listening", "hearing", "thinking", "speaking", "muted"];

test("every voice state names itself, its detail and its artwork", () => {
  const labels = new Set();
  for (const state of states) {
    const value = presence(state);
    assert.equal(value.state, state);
    assert.ok(value.label.length > 0, `${state} has a label`);
    assert.ok(value.detail.length > 0, `${state} has a detail`);
    assert.ok(value.icon.length > 0, `${state} has an icon`);
    assert.ok(value.frames.length > 0, `${state} has artwork`);
    assert.ok(!labels.has(value.label), `${state} reuses the label ${value.label}`);
    assert.equal(value.active, state !== "off");
    labels.add(value.label);
  }
});

test("motion is bounded to animated states and every frame has a cadence", () => {
  for (const state of states) {
    const value = presence(state);
    if (value.frames.length > 1) {
      assert.ok(value.interval > 0, `${state} animates with an interval`);
      assert.ok(value.interval >= 100, `${state} moves slower than 10 frames a second`);
    } else assert.equal(value.interval, 0, `${state} holds a single frame still`);
  }
  assert.deepEqual(presence("listening").frames, ["◉", "◎", "○", "◎"]);
  assert.deepEqual(presence("muted").frames, ["⊘"]);
});

test("an error replaces the label and detail but keeps the running controls", () => {
  const running = presence("thinking", "engine crashed");
  assert.equal(running.state, "error");
  assert.equal(running.label, "Error");
  assert.equal(running.detail, "engine crashed");
  assert.equal(running.tone, "danger");
  assert.equal(running.active, true);
  assert.equal(presence("off", "start failed").active, false);
  assert.ok(PRESENCE_ERROR_MS > 0);
});

test("a status crossing IPC normalizes only names the engine reports", () => {
  assert.equal(normalizeVoiceStatus("hearing"), "hearing");
  assert.equal(normalizeVoiceStatus("muted"), "muted");
  assert.equal(normalizeVoiceStatus(""), "off");
  assert.equal(normalizeVoiceStatus(undefined), "off");
  assert.equal(normalizeVoiceStatus("negotiating"), "off");
});

test("frames wrap around a loop in both directions", () => {
  const value = presence("thinking");
  assert.equal(presenceGlyph(value, 0), value.frames[0]);
  assert.equal(presenceGlyph(value, value.frames.length), value.frames[0]);
  assert.equal(presenceGlyph(value, value.frames.length + 1), value.frames[1]);
  assert.equal(presenceGlyph(value, -1), value.frames[value.frames.length - 1]);
});
