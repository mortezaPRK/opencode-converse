import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { cp, mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { RUNTIME_VERSION, readEngineManifest } from "../dist/client/engine.js";
import { releaseVersion } from "../scripts/release-version.mjs";
import { setVersion } from "../scripts/set-version.mjs";
import {
  TARGETS,
  assetName,
  verifyAssets,
  writeEngineManifest,
} from "../scripts/write-engine-manifest.mjs";

const repo = fileURLToPath(new URL("../", import.meta.url));
const text = (root, path) => readFile(join(root, path), "utf8");

async function copyProject() {
  const root = await mkdtemp(join(tmpdir(), "converse-release-"));
  for (const path of [
    "package.json",
    "package-lock.json",
    "assets/engines.json",
    "native/Cargo.toml",
    "native/Cargo.lock",
    "native/runtime/Cargo.toml",
    ...["ve-audio", "ve-asr", "ve-vad", "ve-turn", "ve-tts"].map(
      (crate) => `native/crates/${crate}/Cargo.toml`,
    ),
    "src/client/engine.ts",
  ]) {
    await mkdir(join(root, path, ".."), { recursive: true });
    await cp(join(repo, path), join(root, path));
  }
  return root;
}

test("release version: an untagged current version is released as it is", () => {
  const input = { current: "0.1.0", tags: [], mode: "stable" };
  assert.equal(releaseVersion(input), "0.1.0");
  assert.equal(releaseVersion({ ...input, tags: ["v0.0.9"] }), "0.1.0");
});

test("release version: a tagged current version moves to the next patch", () => {
  assert.equal(releaseVersion({ current: "0.1.0", tags: ["v0.1.0"], mode: "stable" }), "0.1.1");
  assert.equal(releaseVersion({ current: "1.9.9", tags: ["v1.9.9"], mode: "stable" }), "1.9.10");
});

test("release version: pull request builds append the PR and run number", () => {
  assert.equal(
    releaseVersion({ current: "0.1.0", tags: [], mode: "pr", pr: 7, run: 12 }),
    "0.1.0-pr.7.12",
  );
  assert.equal(
    releaseVersion({ current: "0.1.0", tags: ["v0.1.0"], mode: "pr", pr: 7, run: 12 }),
    "0.1.1-pr.7.12",
  );
});

test("release version rejects invalid input", () => {
  const ok = { current: "0.1.0", tags: [], mode: "pr", pr: 1, run: 1 };
  assert.throws(() => releaseVersion({ ...ok, current: "0.1.0-pr.1.1" }));
  assert.throws(() => releaseVersion({ ...ok, current: "v0.1.0" }));
  assert.throws(() => releaseVersion({ ...ok, mode: "other" }));
  assert.throws(() => releaseVersion({ ...ok, tags: "v0.1.0" }));
  assert.throws(() => releaseVersion({ ...ok, pr: undefined }));
  assert.throws(() => releaseVersion({ ...ok, run: 0 }));
  assert.throws(() => releaseVersion({ ...ok, pr: 1.5 }));
});

test("set-version updates every pinned place and is idempotent", async () => {
  const root = await copyProject();
  for (const version of ["2.3.4-pr.9.1", "2.3.4-pr.9.1"]) {
    setVersion(root, version);
    const pkg = JSON.parse(await text(root, "package.json"));
    const lock = JSON.parse(await text(root, "package-lock.json"));
    assert.equal(pkg.version, version);
    assert.equal(lock.version, version);
    assert.equal(lock.packages[""].version, version);
    assert.equal(JSON.parse(await text(root, "assets/engines.json")).version, version);
    assert.match(await text(root, "native/Cargo.toml"), new RegExp(`\\nversion = "${version}"`));
    assert.match(
      await text(root, "src/client/engine.ts"),
      new RegExp(`RUNTIME_VERSION = "${version}"`),
    );
    const cargoLock = await text(root, "native/Cargo.lock");
    const mine = [
      ...cargoLock.matchAll(/name = "([^"]+)"\nversion = "([^"]+)"\n(?!source)/g),
    ].filter((m) => m[1] === "opencode-converse" || m[1].startsWith("ve-"));
    assert.equal(mine.length, 6);
    assert.ok(mine.every((m) => m[2] === version));
    assert.match(cargoLock, /name = "webrtc-audio-processing-sys"\nversion = "2\.1\.0"\n/);
  }
  assert.throws(() => setVersion(root, "1.2"));
  assert.throws(() => setVersion(root, "v1.2.3"));
});

test("all pinned versions agree", async () => {
  const pkg = JSON.parse(await text(repo, "package.json"));
  const lock = JSON.parse(await text(repo, "package-lock.json"));
  assert.equal(lock.version, pkg.version);
  assert.equal(lock.packages[""].version, pkg.version);
  assert.equal(RUNTIME_VERSION, pkg.version);
  assert.equal(JSON.parse(await text(repo, "assets/engines.json")).version, pkg.version);
  assert.match(await text(repo, "native/Cargo.toml"), new RegExp(`\\nversion = "${pkg.version}"`));
  const cargoLock = await text(repo, "native/Cargo.lock");
  for (const name of ["opencode-converse", "ve-asr", "ve-audio", "ve-tts", "ve-turn", "ve-vad"])
    assert.ok(cargoLock.includes(`name = "${name}"\nversion = "${pkg.version}"`), name);
});

async function artifacts(root, skip) {
  const dir = join(root, "artifacts");
  for (const { target, exe } of TARGETS) {
    if (target === skip) continue;
    await mkdir(join(dir, `engine-${target}`), { recursive: true });
    await writeFile(join(dir, `engine-${target}`, exe), `engine ${target}`);
  }
  return dir;
}

test("the manifest writer names, hashes, and self-validates the assets", async () => {
  const root = await copyProject();
  const out = join(root, "out");
  const result = writeEngineManifest({
    root,
    version: RUNTIME_VERSION,
    artifacts: await artifacts(root),
    out,
    readManifest: readEngineManifest,
  });
  for (const entry of TARGETS) {
    const asset = result.engines[entry.target];
    assert.equal(asset.file, assetName(RUNTIME_VERSION, entry));
    const body = await readFile(join(out, asset.file));
    assert.equal(asset.bytes, body.length);
    assert.equal(asset.sha256, createHash("sha256").update(body).digest("hex"));
  }
  assert.equal(
    JSON.parse(await text(root, "assets/engines.json")).engines["win32-x64"].file.endsWith(".exe"),
    true,
  );
  assert.equal(Object.keys(readEngineManifest(root)).length, 3);
  verifyAssets({ root, dir: out });
  await writeFile(join(out, result.engines["linux-x64"].file), "tampered!!!!!!!!");
  assert.throws(() => verifyAssets({ root, dir: out }), /mismatch/);
});

test("the manifest writer fails when a target is missing or the version is invalid", async () => {
  const root = await copyProject();
  await assert.rejects(
    async () =>
      writeEngineManifest({
        root,
        version: RUNTIME_VERSION,
        artifacts: await artifacts(root, "win32-x64"),
        out: join(root, "o"),
      }),
    /win32-x64/,
  );
  assert.throws(() =>
    writeEngineManifest({
      root,
      version: "nope",
      artifacts: join(root, "artifacts"),
      out: join(root, "o"),
    }),
  );
});
