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
  assert.ok(!pkg.files.some((file) => file.startsWith("assets/bin")));
  assert.ok(!pkg.scripts.prepack.includes("stage"));
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
    assert.ok(["file", "tar.bz2", "files"].includes(asset.format));
    assert.ok(asset.runtime.path && !asset.runtime.path.startsWith("/"));
    const pins =
      asset.format === "files"
        ? asset.files.map((member) => ({ ...member, file: member.path }))
        : [asset];
    assert.ok(pins.length > 0);
    if (asset.format === "files") {
      assert.ok(!asset.runtime.path.includes("/") && !asset.url && !asset.sha256);
    }
    for (const pin of pins) {
      assert.match(pin.sha256, /^[a-f0-9]{64}$/);
      assert.equal(new URL(pin.url).protocol, "https:");
      assert.ok(Number.isSafeInteger(pin.bytes) && pin.bytes > 0 && pin.bytes < 200_000_000);
      assert.ok(!pin.file.includes("/") && !pin.file.includes("\\"));
      if (pin.url.includes("huggingface.co")) assert.match(pin.url, /\/resolve\/[a-f0-9]{40}\//);
    }
    if (asset.format === "tar.bz2") {
      assert.ok(asset.install?.root);
      assert.ok(asset.runtime.tokens?.startsWith(`tts/${asset.install.root}/`));
      assert.ok(asset.runtime.dataDir?.startsWith(`tts/${asset.install.root}/`));
    }
  }
});
