import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  RUNTIME_VERSION,
  engineAssetName,
  engineExecutable,
  ensureEngine,
  isVersion,
  readEngineManifest,
} from "../dist/client/engine.js";

const repo = fileURLToPath(new URL("../", import.meta.url));
const { platform, arch } = process;
const BODY = Buffer.from("#!/bin/sh\nexit 0\n".repeat(200));
const sha = (buffer) => createHash("sha256").update(buffer).digest("hex");

async function fixture(entryOverride, manifestOverride) {
  const root = await mkdtemp(join(tmpdir(), "converse-install-"));
  const pkg = join(root, "pkg");
  await mkdir(join(pkg, "assets"), { recursive: true });
  await mkdir(join(pkg, "native"), { recursive: true });
  await writeFile(join(pkg, "native", "THIRD_PARTY.md"), "notices");
  const entry = {
    file: engineAssetName(platform, arch),
    bytes: BODY.length,
    sha256: sha(BODY),
    ...entryOverride,
  };
  await writeFile(
    join(pkg, "assets", "engines.json"),
    JSON.stringify({
      version: RUNTIME_VERSION,
      engines: { [`${platform}-${arch}`]: entry },
      ...manifestOverride,
    }),
  );
  const home = join(root, "home");
  await mkdir(home);
  return { root, options: { env: {}, platform, arch, home, packageRoot: pkg } };
}

async function serve(handler) {
  const hits = [];
  const server = createServer((req, res) => {
    hits.push(req.url);
    handler(req, res);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    base: `http://127.0.0.1:${server.address().port}`,
    hits,
    close: () => {
      server.closeAllConnections();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}
const ok = (body) => (_req, res) => {
  res.writeHead(200, { "content-length": body.length });
  res.end(body);
};
const leftovers = (options) => {
  const bin = dirname(dirname(engineExecutable(options)));
  return existsSync(bin) ? readdirSync(bin).filter((name) => name !== RUNTIME_VERSION) : [];
};

test("downloads, verifies, marks, and reports progress", async () => {
  const { root, options } = await fixture();
  const s = await serve(ok(BODY));
  const progress = [];
  try {
    const path = await ensureEngine({
      ...options,
      releaseBase: s.base,
      onProgress: (text) => progress.push(text),
    });
    assert.equal(path, engineExecutable(options));
    assert.deepEqual(await readFile(path), BODY);
    assert.ok(existsSync(join(dirname(path), ".complete")));
    assert.equal(await readFile(join(dirname(path), "THIRD_PARTY.md"), "utf8"), "notices");
    if (platform !== "win32") assert.ok(((await stat(path)).mode & 0o111) !== 0);
    assert.deepEqual(s.hits, [`/v${RUNTIME_VERSION}/${engineAssetName(platform, arch)}`]);
    assert.ok(progress.length >= 1 && progress.length <= 3);
    assert.match(progress[0], /Downloading voice engine/);
    assert.deepEqual(leftovers(options), []);
  } finally {
    await s.close();
    await rm(root, { recursive: true, force: true });
  }
});

test("hash mismatch, oversize, and truncation install nothing", async () => {
  const cases = [
    ["SHA-256", ok(Buffer.alloc(BODY.length, 1))],
    [
      "more than the pinned",
      (_req, res) => {
        res.writeHead(200);
        res.end(Buffer.concat([BODY, BODY]));
      },
    ],
    [
      "incomplete download",
      (_req, res) => {
        res.write(BODY.subarray(0, 100));
        setTimeout(() => res.destroy(), 20);
      },
    ],
  ];
  for (const [message, handler] of cases) {
    const { root, options } = await fixture();
    const s = await serve(handler);
    try {
      await assert.rejects(
        ensureEngine({ ...options, releaseBase: s.base }),
        message === "incomplete download"
          ? /Failed to download the voice engine from http:\/\/127\.0\.0\.1/
          : new RegExp(message),
      );
      assert.ok(!existsSync(dirname(engineExecutable(options))));
      assert.deepEqual(leftovers(options), []);
    } finally {
      await s.close();
      await rm(root, { recursive: true, force: true });
    }
  }
});

test("stalled and unreachable servers fail with the URL", async () => {
  const { root, options } = await fixture();
  const s = await serve(() => {});
  try {
    await assert.rejects(
      ensureEngine({ ...options, releaseBase: s.base, connectTimeoutMs: 100 }),
      /no response within 100 ms/,
    );
    const base = s.base;
    await s.close();
    await assert.rejects(ensureEngine({ ...options, releaseBase: base }), new RegExp(base));
    assert.deepEqual(leftovers(options), []);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("HTTP errors and unsupported platforms are clear", async () => {
  const { root, options } = await fixture();
  const s = await serve((_req, res) => {
    res.writeHead(404);
    res.end();
  });
  try {
    await assert.rejects(ensureEngine({ ...options, releaseBase: s.base }), /HTTP 404/);
    await assert.rejects(
      ensureEngine({ ...options, arch: "mips" }),
      /No voice engine is published for .*-mips.*engine:build && npm run engine:stage/,
    );
    assert.equal(s.hits.length, 1);
  } finally {
    await s.close();
    await rm(root, { recursive: true, force: true });
  }
});

test("manifest is validated strictly", async () => {
  const bad = [
    [{}, { version: "9.9.9" }, /does not match/],
    [{}, { extra: 1 }, /exactly version and engines/],
    [{}, { engines: { "plan9-arm": {} } }, /unknown target/],
    [{ sha256: "AB" }, {}, /invalid sha256/],
    [{ bytes: 256 * 1024 * 1024 + 1 }, {}, /invalid size/],
    [{ file: "evil" }, {}, /unexpected file name/],
    [{ extra: 1 }, {}, /exactly file, bytes, sha256/],
  ];
  for (const [entry, manifest, pattern] of bad) {
    const { root, options } = await fixture(entry, manifest);
    try {
      assert.throws(() => readEngineManifest(options.packageRoot), pattern);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }
  const { root, options } = await fixture();
  await writeFile(join(options.packageRoot, "assets", "engines.json"), "{");
  try {
    assert.throws(() => readEngineManifest(options.packageRoot), /Cannot read/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
  // The committed placeholder is valid and empty.
  assert.deepEqual(readEngineManifest(repo), {});
});

test("an existing complete install is reused without the network", async () => {
  const { root, options } = await fixture();
  const s = await serve(ok(BODY));
  try {
    const path = await ensureEngine({ ...options, releaseBase: s.base });
    await writeFile(path, "changed");
    const failing = () => assert.fail("network used");
    assert.equal(await ensureEngine({ ...options, fetch: failing }), path);
    assert.equal(await readFile(path, "utf8"), "changed");
    assert.equal(s.hits.length, 1);
  } finally {
    await s.close();
    await rm(root, { recursive: true, force: true });
  }
});

test("concurrent installs share one download and one valid result", async () => {
  const { root, options } = await fixture();
  const s = await serve((_req, res) => setTimeout(() => ok(BODY)(_req, res), 150));
  try {
    const paths = await Promise.all(
      Array.from({ length: 4 }, () => ensureEngine({ ...options, releaseBase: s.base })),
    );
    assert.equal(new Set(paths).size, 1);
    assert.deepEqual(await readFile(paths[0]), BODY);
    assert.equal(s.hits.length, 1);
    assert.deepEqual(leftovers(options), []);
  } finally {
    await s.close();
    await rm(root, { recursive: true, force: true });
  }
});

test("version strings accept full semver with prereleases", () => {
  for (const v of ["0.7.0", "0.1.1-pr.43.7", "1.0.0-rc.1", "1.0.0-0.alpha-1"])
    assert.ok(isVersion(v), v);
  for (const v of ["1.0", "01.0.0", "1.0.0-", "1.0.0+build", "../1.0.0", "1.0.0-a/b", 3])
    assert.ok(!isVersion(v), String(v));
  assert.equal(
    engineAssetName("win32", "x64", "0.1.1-pr.43.7"),
    "opencode-converse-0.1.1-pr.43.7-win32-x64.exe",
  );
});

test("engine:stage installs into the state folder", async () => {
  const root = await mkdtemp(join(tmpdir(), "converse-stage-"));
  const home = join(root, "home");
  await mkdir(home);
  const source = join(root, "engine");
  await writeFile(source, "#!/bin/sh\nexit 0\n");
  const env = { ...process.env, HOME: home, XDG_DATA_HOME: join(home, "data"), LOCALAPPDATA: home };
  try {
    const { stage } = await import(pathToFileURL(join(repo, "native/scripts/stage.mjs")).href);
    assert.equal(await stage({ env, source }), 0);
    const installed = engineExecutable({ home, env });
    assert.ok(installed.startsWith(home));
    assert.ok(existsSync(join(dirname(installed), ".complete")));
    assert.ok(existsSync(join(dirname(installed), "THIRD_PARTY.md")));
    assert.ok(!existsSync(join(repo, "assets", "bin")));
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("npm pack ships the manifest and notices but no engine", () => {
  const out = JSON.parse(
    execFileSync("npm", ["pack", "--dry-run", "--json", "--ignore-scripts"], {
      cwd: repo,
      encoding: "utf8",
      shell: process.platform === "win32",
    }),
  );
  const files = out[0].files.map((file) => file.path);
  assert.ok(files.includes("assets/engines.json"));
  assert.ok(files.includes("native/THIRD_PARTY.md"));
  assert.ok(!files.some((file) => file.startsWith("assets/bin")));
});
