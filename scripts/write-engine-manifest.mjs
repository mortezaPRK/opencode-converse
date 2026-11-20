// Collect downloaded engine artifacts into release assets and write assets/engines.json.
//   node scripts/write-engine-manifest.mjs write <version> <artifacts-dir> <assets-out-dir>
//   node scripts/write-engine-manifest.mjs verify <assets-dir>
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { isSemver } from "./set-version.mjs";

export const TARGETS = [
  { target: "darwin-arm64", platform: "darwin", arch: "arm64", exe: "opencode-converse" },
  { target: "linux-x64", platform: "linux", arch: "x64", exe: "opencode-converse" },
  { target: "win32-x64", platform: "win32", arch: "x64", exe: "opencode-converse.exe" },
];

export const assetName = (version, { platform, arch }) =>
  `opencode-converse-${version}-${platform}-${arch}${platform === "win32" ? ".exe" : ""}`;

const sha256 = (path) => createHash("sha256").update(readFileSync(path)).digest("hex");

/**
 * Artifacts are laid out as `<artifacts>/engine-<target>/<exe>` (download-artifact
 * with a pattern). Every target is required.
 */
export function writeEngineManifest({ root, version, artifacts, out, readManifest }) {
  if (!isSemver(version)) throw new Error(`Invalid version: ${String(version)}`);
  mkdirSync(out, { recursive: true });
  const engines = {};
  for (const entry of TARGETS) {
    const source = join(artifacts, `engine-${entry.target}`, entry.exe);
    if (!existsSync(source)) throw new Error(`Missing engine artifact for ${entry.target}`);
    const bytes = statSync(source).size;
    if (bytes <= 0) throw new Error(`Engine artifact for ${entry.target} is empty`);
    const file = assetName(version, entry);
    copyFileSync(source, join(out, file));
    engines[entry.target] = { file, bytes, sha256: sha256(source) };
  }
  const path = join(root, "assets", "engines.json");
  writeFileSync(path, `${JSON.stringify({ version, engines }, null, 2)}\n`);
  if (readManifest) {
    const read = readManifest(root);
    for (const { target } of TARGETS)
      if (!read[target]) throw new Error(`Manifest validation lost ${target}`);
  }
  return { version, engines };
}

/** Check that every manifest asset in `dir` has the pinned size and hash. */
export function verifyAssets({ root, dir }) {
  const manifest = JSON.parse(readFileSync(join(root, "assets", "engines.json"), "utf8"));
  for (const { target } of TARGETS) {
    const entry = manifest.engines?.[target];
    if (!entry) throw new Error(`engines.json has no ${target}`);
    const path = join(dir, entry.file);
    if (!existsSync(path)) throw new Error(`Missing asset ${entry.file}`);
    if (statSync(path).size !== entry.bytes) throw new Error(`Size mismatch for ${entry.file}`);
    if (sha256(path) !== entry.sha256) throw new Error(`Hash mismatch for ${entry.file}`);
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const [command, ...rest] = process.argv.slice(2);
  try {
    if (command === "write" && rest.length === 3) {
      // Self-check with the same strict reader the plugin uses (needs `npm run build`).
      const { readEngineManifest } = await import(`${root}dist/client/engine.js`);
      const result = writeEngineManifest({
        root,
        version: rest[0],
        artifacts: rest[1],
        out: rest[2],
        readManifest: readEngineManifest,
      });
      for (const [target, e] of Object.entries(result.engines))
        console.log(`${target}: ${e.file} ${e.bytes} ${e.sha256}`);
    } else if (command === "verify" && rest.length === 1) {
      verifyAssets({ root, dir: rest[0] });
      console.log("Release assets match engines.json");
    } else {
      console.error("usage: write <version> <artifacts-dir> <out-dir> | verify <assets-dir>");
      process.exit(2);
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }
}
