import assert from "node:assert/strict";
import { readFile, stat } from "node:fs/promises";
import test from "node:test";
import { RUNTIME_VERSION } from "../dist/client/engine.js";

const root = new URL("../", import.meta.url);

test("the published package exposes the three host parts and RPC contract", async () => {
  const pkg = JSON.parse(await readFile(new URL("package.json", root), "utf8"));
  assert.equal(pkg.name, "@mortz.prk/opencode-converse");
  for (const key of [".", "./tui", "./gui-experimental", "./rpc"]) {
    const entry = pkg.exports[key];
    const file = typeof entry === "string" ? entry : entry.import;
    assert.ok((await stat(new URL(file, root))).isFile());
  }
  assert.ok(!pkg.files.includes("native"));
  assert.ok(!pkg.scripts.postinstall);
  assert.ok(pkg.files.includes("assets/bin"));
  assert.equal(RUNTIME_VERSION, pkg.version);
});

test("every speech asset has a bounded size and pinned SHA-256", async () => {
  const manifest = JSON.parse(await readFile(new URL("assets/models.json", root), "utf8"));
  assert.equal(manifest.schema, 1);
  assert.equal(manifest.language, "en");
  assert.equal(new Set(manifest.assets.map((asset) => asset.id)).size, manifest.assets.length);
  const ids = new Set(manifest.assets.map((asset) => asset.id));
  for (const role of ["vad", "turn", "asr", "tts"]) assert.ok(ids.has(manifest.roles[role]));
  for (const asset of manifest.assets) {
    assert.match(asset.sha256, /^[a-f0-9]{64}$/);
    assert.equal(new URL(asset.url).protocol, "https:");
    assert.ok(Number.isSafeInteger(asset.bytes) && asset.bytes > 0 && asset.bytes < 200_000_000);
    assert.ok(!asset.file.includes("/") && !asset.file.includes("\\"));
    assert.ok(["file", "tar.bz2"].includes(asset.format));
    assert.ok(asset.runtime.path && !asset.runtime.path.startsWith("/"));
    if (asset.format === "tar.bz2") {
      assert.ok(asset.install?.root);
      assert.ok(asset.runtime.tokens?.startsWith(`tts/${asset.install.root}/`));
      assert.ok(asset.runtime.dataDir?.startsWith(`tts/${asset.install.root}/`));
    }
    if (asset.url.includes("huggingface.co")) assert.match(asset.url, /\/resolve\/[a-f0-9]{40}\//);
  }
});
