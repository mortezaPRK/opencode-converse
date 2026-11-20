import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { ensureModels } from "../dist/client/models.js";

const posix = process.platform !== "win32";
const skip = posix ? false : "fake engine scripts need a POSIX shell-free node shebang";

/** A fake engine: `status` and `install` bodies are JS evaluated inside a node script. */
async function fake({ status, install }) {
  const dir = await mkdtemp(join(tmpdir(), "converse-models-"));
  const exe = join(dir, "engine");
  const log = join(dir, "calls.log");
  const marker = join(dir, "installed");
  await writeFile(
    exe,
    `#!${process.execPath}
const fs = require("node:fs");
const log = ${JSON.stringify(log)};
const marker = ${JSON.stringify(marker)};
const out = (v) => process.stdout.write(JSON.stringify(v) + "\\n");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const args = process.argv.slice(2).join(" ");
fs.appendFileSync(log, args + "\\n");
(async () => {
  if (args === "models status --json") { ${status} }
  else if (args === "models install --json") { ${install} }
  else process.exit(2);
})();
`,
  );
  await chmod(exe, 0o755);
  return {
    exe,
    dir,
    calls: async () => (existsSync(log) ? (await readFile(log, "utf8")).trim().split("\n") : []),
    marker,
  };
}
const installedStatus = `out({type:"status",installed:true,missing_bytes:0,assets:[]});`;
const missingStatus = `out({type:"status",installed:false,missing_bytes:220200960,assets:[]});`;

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

test("already installed models do not run the installer", { skip }, async (t) => {
  const f = await fake({ status: installedStatus, install: `process.exit(9)` });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  const progress = [];
  await ensureModels({ executable: f.exe, onProgress: (x) => progress.push(x) });
  assert.deepEqual(await f.calls(), ["models status --json"]);
  assert.deepEqual(progress, []);
});

test("missing models install with throttled MB progress", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `
      out({type:"start",asset:"a",bytes:2097152});
      out({type:"progress",asset:"a",done:1048576,total:2097152});
      out({type:"progress",asset:"a",done:1048576,total:2097152});
      out({type:"progress",asset:"a",done:2097152,total:2097152});
      out({type:"verified",asset:"a"});
      out({type:"unknown-future-event"});
      out({type:"done"});`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  const progress = [];
  await ensureModels({ executable: f.exe, onProgress: (x) => progress.push(x) });
  assert.deepEqual(await f.calls(), ["models status --json", "models install --json"]);
  assert.deepEqual(progress, [
    "Downloading speech models",
    "Downloading speech models 1/2 MB",
    "Downloading speech models 2/2 MB",
  ]);
});

test("waiting for another installer is visible", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `out({type:"waiting"}); out({type:"done"});`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  const progress = [];
  await ensureModels({ executable: f.exe, onProgress: (x) => progress.push(x) });
  assert.match(progress.at(-1), /Waiting for another window/);
});

test("install failure names the asset, the reason, and the manual command", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `out({type:"start",asset:"phonon-31m-en",bytes:1});
      out({type:"error",asset:"phonon-31m-en",message:"HTTP 503"}); process.exit(1);`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(ensureModels({ executable: f.exe }), (error) => {
    assert.match(error.message, /phonon-31m-en/);
    assert.match(error.message, /HTTP 503/);
    assert.ok(error.message.includes(`"${f.exe}" models install`));
    return true;
  });
});

test("a crash without an error event reports the exit and stderr", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `process.stderr.write("disk full"); process.exit(3);`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(ensureModels({ executable: f.exe }), /disk full[\s\S]*models install/);
});

test("a failing status check is reported with the manual command", { skip }, async (t) => {
  const f = await fake({ status: `process.exit(1)`, install: `out({type:"done"})` });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(
    ensureModels({ executable: f.exe }),
    /Could not check speech models[\s\S]*models install/,
  );
  assert.deepEqual(await f.calls(), ["models status --json"]);
});

test("a missing engine binary is a clear error", { skip }, async () => {
  await assert.rejects(
    ensureModels({ executable: join(tmpdir(), "converse-no-such-engine") }),
    /Could not check speech models: could not start the engine/,
  );
});

test(
  "cancelling during install kills the child and rejects with the reason",
  { skip },
  async (t) => {
    const f = await fake({
      status: missingStatus,
      install: `fs.writeFileSync(marker, String(process.pid));
      out({type:"progress",asset:"a",done:1,total:2}); setInterval(() => {}, 1000);`,
    });
    t.after(() => rm(f.dir, { recursive: true, force: true }));
    const abort = new AbortController();
    const reason = new Error("stopped by user");
    const done = ensureModels({
      executable: f.exe,
      signal: abort.signal,
      onProgress: (text) => {
        if (text.endsWith("MB")) abort.abort(reason);
      },
    });
    await assert.rejects(done, (error) => error === reason);
    const pid = Number(await readFile(f.marker, "utf8"));
    assert.equal(alive(pid), false);
  },
);

test("an already-aborted signal never starts the engine", { skip }, async (t) => {
  const f = await fake({ status: installedStatus, install: `` });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  const abort = new AbortController();
  abort.abort(new Error("no"));
  await assert.rejects(ensureModels({ executable: f.exe, signal: abort.signal }), /no/);
  assert.deepEqual(await f.calls(), []);
});

test("malformed output kills the installer", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `fs.writeFileSync(marker, String(process.pid)); process.stdout.write("not json\\n"); setInterval(() => {}, 1000);`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(ensureModels({ executable: f.exe }), /malformed progress output/);
  assert.equal(alive(Number(await readFile(f.marker, "utf8"))), false);
});

test("oversized output lines are rejected without buffering them", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `fs.writeFileSync(marker, String(process.pid)); process.stdout.write("x".repeat(1 << 20)); setInterval(() => {}, 1000);`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(ensureModels({ executable: f.exe }), /oversized output line/);
  assert.equal(alive(Number(await readFile(f.marker, "utf8"))), false);
});

test("an installer that goes silent is killed by the idle timeout", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `fs.writeFileSync(marker, String(process.pid)); setInterval(() => {}, 1000);`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(ensureModels({ executable: f.exe, idleTimeoutMs: 200 }), /no output for/);
  assert.equal(alive(Number(await readFile(f.marker, "utf8"))), false);
});

test("the overall timeout bounds a chatty installer", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `setInterval(() => out({type:"progress",done:1,total:2}), 20);`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await assert.rejects(
    ensureModels({ executable: f.exe, totalTimeoutMs: 300 }),
    /did not finish within/,
  );
});

test("the installer never sees server credentials", { skip }, async (t) => {
  const f = await fake({
    status: missingStatus,
    install: `fs.writeFileSync(marker, JSON.stringify(Object.keys(process.env))); out({type:"done"});`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  const names = ["OPENCODE_SERVER_PASSWORD", "OPENCODE_SERVER_USERNAME", "ANTHROPIC_API_KEY"];
  for (const name of names) process.env[name] = "secret";
  t.after(() => names.forEach((name) => delete process.env[name]));
  await ensureModels({ executable: f.exe });
  const keys = JSON.parse(await readFile(f.marker, "utf8"));
  for (const name of names) assert.ok(!keys.includes(name));
});

test("two concurrent starts both succeed against a serialising installer", { skip }, async (t) => {
  // The engine's own lock queues installers; the later one finds everything installed.
  const f = await fake({
    status: `out({type:"status",installed:fs.existsSync(marker),missing_bytes:1,assets:[]});`,
    install: `
      const lock = marker + ".lock";
      for (;;) {
        try { fs.writeFileSync(lock, "", { flag: "wx" }); break; } catch { out({type:"waiting"}); await sleep(30); }
      }
      if (!fs.existsSync(marker)) { await sleep(200); fs.writeFileSync(marker, "1"); fs.appendFileSync(marker + ".count", "x"); }
      fs.rmSync(lock); out({type:"done"});`,
  });
  t.after(() => rm(f.dir, { recursive: true, force: true }));
  await Promise.all([ensureModels({ executable: f.exe }), ensureModels({ executable: f.exe })]);
  assert.equal((await readFile(f.marker + ".count", "utf8")).length, 1);
});
