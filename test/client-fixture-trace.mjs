import assert from "node:assert/strict";

/** Explicit --test-fixture telemetry only; no production transport/parser change. */
export function fixtureTrace(child, errors = []) {
  const played = [];
  let pending = [];
  let pendingBytes = 0;
  let failed = false;
  let finished = false;
  let finish;
  const closed = new Promise((resolve) => {
    finish = resolve;
  });
  child.stderr.on("data", (chunk) => {
    if (failed) return;
    try {
      let start = 0;
      for (let index = 0; index < chunk.length; index++) {
        if (chunk[index] !== 10) continue;
        assert.ok(
          pendingBytes + index - start <= 65_536,
          "fixture trace record must remain bounded",
        );
        if (index > start) pending.push(chunk.subarray(start, index));
        pendingBytes += index - start;
        const frame = Buffer.concat(pending, pendingBytes);
        pending = [];
        pendingBytes = 0;
        start = index + 1;
        const record = JSON.parse(frame.toString("utf8"));
        assert.equal(record.fixture, "played");
        assert.equal(typeof record.text, "string");
        played.push(record.text);
      }
      assert.ok(
        pendingBytes + chunk.length - start <= 65_536,
        "fixture trace fragment must remain bounded",
      );
      if (start < chunk.length) {
        pending.push(Buffer.from(chunk.subarray(start)));
        pendingBytes += chunk.length - start;
      }
    } catch (error) {
      failed = true;
      errors.push(error);
    }
  });
  const done = () => {
    if (finished) return;
    finished = true;
    if (pendingBytes) errors.push(new Error("fixture trace ended with a partial record"));
    finish();
  };
  child.stderr.once("end", done);
  child.stderr.once("close", done);
  child.stderr.once("error", (error) => {
    errors.push(error);
    done();
  });
  return { played, closed, errors };
}
