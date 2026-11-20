// Set the one release version everywhere it is pinned. Idempotent.
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$/;

export const isSemver = (value) =>
  typeof value === "string" && value.length <= 64 && SEMVER.test(value);

const read = (root, path) => readFileSync(join(root, path), "utf8");
const write = (root, path, text) => writeFileSync(join(root, path), text);

function replaceOnce(text, pattern, replacement, what) {
  if (!pattern.test(text)) throw new Error(`Cannot find ${what}`);
  return text.replace(pattern, replacement);
}

function setJson(root, path, version, edit) {
  const value = JSON.parse(read(root, path));
  edit(value);
  write(root, path, `${JSON.stringify(value, null, 2)}\n`);
}

/** Workspace crates are the Cargo.lock packages that have no registry source. */
function setCargoLock(text, version) {
  let count = 0;
  const out = text.replace(/\[\[package\]\]\n([\s\S]*?)(?=\n\[\[package\]\]|\n*$)/g, (block) => {
    if (/^source = /m.test(block)) return block;
    count++;
    return block.replace(/^version = ".*"$/m, `version = "${version}"`);
  });
  if (count === 0) throw new Error("Cannot find workspace crates in native/Cargo.lock");
  return out;
}

export function setVersion(root, version) {
  if (!isSemver(version)) throw new Error(`Invalid version: ${String(version)}`);
  setJson(root, "package.json", version, (pkg) => {
    pkg.version = version;
  });
  setJson(root, "package-lock.json", version, (lock) => {
    lock.version = version;
    if (!lock.packages?.[""]) throw new Error("package-lock.json has no root package entry");
    lock.packages[""].version = version;
  });
  setJson(root, "assets/engines.json", version, (manifest) => {
    manifest.version = version;
  });
  write(
    root,
    "native/Cargo.toml",
    replaceOnce(
      read(root, "native/Cargo.toml"),
      /(\[workspace\.package\][^[]*?\nversion = )"[^"]*"/,
      `$1"${version}"`,
      "the workspace version in native/Cargo.toml",
    ),
  );
  write(root, "native/Cargo.lock", setCargoLock(read(root, "native/Cargo.lock"), version));
  write(
    root,
    "src/client/engine.ts",
    replaceOnce(
      read(root, "src/client/engine.ts"),
      /(export const RUNTIME_VERSION = )"[^"]*"/,
      `$1"${version}"`,
      "RUNTIME_VERSION in src/client/engine.ts",
    ),
  );
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const version = process.argv[2];
  if (!isSemver(version)) {
    console.error("usage: node scripts/set-version.mjs <semver>");
    process.exit(2);
  }
  setVersion(fileURLToPath(new URL("../", import.meta.url)), version);
  console.log(`Version set to ${version}`);
}
