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

/** Package names of the workspace members; patched path dependencies keep their own version. */
function workspaceCrates(root) {
  const manifest = read(root, "native/Cargo.toml");
  const members = manifest.match(/^members = \[([^\]]*)\]/m);
  if (!members) throw new Error("Cannot find workspace members in native/Cargo.toml");
  return new Set(
    [...members[1].matchAll(/"([^"]+)"/g)].map(([, member]) => {
      const name = read(root, join("native", member, "Cargo.toml")).match(/^name = "([^"]+)"$/m);
      if (!name) throw new Error(`Cannot find the package name of native/${member}`);
      return name[1];
    }),
  );
}

function setCargoLock(text, crates, version) {
  const found = new Set();
  const out = text.replace(/\[\[package\]\]\n([\s\S]*?)(?=\n\[\[package\]\]|\n*$)/g, (block) => {
    const name = block.match(/^name = "([^"]+)"$/m)?.[1];
    if (!name || !crates.has(name) || /^source = /m.test(block)) return block;
    found.add(name);
    return block.replace(/^version = ".*"$/m, `version = "${version}"`);
  });
  const missing = [...crates].filter((name) => !found.has(name));
  if (missing.length) throw new Error(`Cannot find ${missing.join(", ")} in native/Cargo.lock`);
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
  write(
    root,
    "native/Cargo.lock",
    setCargoLock(read(root, "native/Cargo.lock"), workspaceCrates(root), version),
  );
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
