import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import test from "node:test";
import { fixtureTrace } from "./client-fixture-trace.mjs";

test("fixture consumption trace preserves split UTF-8 records and waits for stderr completion", async () => {
  const child = { stderr: new PassThrough() };
  const trace = fixtureTrace(child);
  const bytes = Buffer.from(JSON.stringify({ fixture: "played", text: "summary €" }) + "\n");
  const split = bytes.indexOf(Buffer.from("€")) + 1;
  child.stderr.write(bytes.subarray(0, split));
  assert.deepEqual(trace.played, []);
  child.stderr.write(bytes.subarray(split));
  child.stderr.end(JSON.stringify({ fixture: "played", text: "drain sentinel" }) + "\n");
  await trace.closed;
  assert.deepEqual(trace.played, ["summary €", "drain sentinel"]);
  assert.deepEqual(trace.errors, []);
});

test("fixture consumption trace reports incomplete stderr records", async () => {
  const child = { stderr: new PassThrough() };
  const trace = fixtureTrace(child);
  child.stderr.end('{"fixture":"played"');
  await trace.closed;
  assert.match(trace.errors[0].message, /partial record/);
});

test("fixture trace accepts large batches of individually bounded complete records", async () => {
  const child = { stderr: new PassThrough() };
  const trace = fixtureTrace(child);
  const records = Array.from({ length: 400 }, (_, epoch) => ({
    fixture: "played",
    text: "x".repeat(320),
    epoch,
  }));
  const bytes = Buffer.from(records.map((record) => JSON.stringify(record) + "\n").join(""));
  for (let offset = 0; offset < bytes.length; offset += 65_536)
    child.stderr.write(bytes.subarray(offset, offset + 65_536));
  child.stderr.end();
  await trace.closed;
  assert.deepEqual(trace.errors, []);
  assert.deepEqual(
    trace.played,
    records.map((record) => record.text),
  );
});

test("fixture trace still rejects one oversized record or unterminated fragment", async () => {
  for (const terminated of [false, true]) {
    const child = { stderr: new PassThrough() };
    const trace = fixtureTrace(child);
    const record = JSON.stringify({ fixture: "played", text: "x".repeat(65_536) });
    child.stderr.end(record + (terminated ? "\n" : ""));
    await trace.closed;
    assert.match(trace.errors[0].message, /bounded/);
    assert.deepEqual(trace.played, []);
  }
});
