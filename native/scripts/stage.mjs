// Stage the release engine and the shared libraries it actually loads for npm packaging.
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { execFileSync, spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { basename, join, resolve } from "node:path";

const native = fileURLToPath(new URL("../", import.meta.url));
const root = fileURLToPath(new URL("../../", import.meta.url));
const platform = process.platform;
const arch = process.arch;
const executable = platform === "win32" ? "opencode-converse.exe" : "opencode-converse";
const destination = join(root, "assets", "bin", `${platform}-${arch}`);
const candidate = /^(lib)?(sherpa-onnx|onnxruntime).*\.(dylib|so|dll)(\..*)?$/;

function targetDirectory(env) {
  const metadata = spawnSync(
    "cargo",
    [
      "metadata",
      "--locked",
      "--no-deps",
      "--format-version",
      "1",
      "--manifest-path",
      join(native, "Cargo.toml"),
    ],
    { encoding: "utf8", env },
  );
  if (metadata.error || metadata.status !== 0) {
    console.error(metadata.error?.message ?? metadata.stderr);
    process.exit(metadata.status ?? 1);
  }
  return JSON.parse(metadata.stdout).target_directory;
}

function dependencies(file) {
  try {
    if (platform === "darwin")
      return execFileSync("otool", ["-L", file], { encoding: "utf8" })
        .split("\n")
        .slice(1)
        .map((line) => line.trim().split(" ")[0])
        .filter(Boolean)
        .map((value) => basename(value));
    if (platform === "linux")
      return execFileSync("readelf", ["-d", file], { encoding: "utf8" })
        .split("\n")
        .map((line) => /NEEDED.*\[(.+)\]/.exec(line)?.[1])
        .filter(Boolean);
  } catch {
    // Dependency tooling is optional; the fallback below stages every candidate library.
  }
  return undefined;
}

function requiredLibraries(release, names, source) {
  if (platform === "win32") return names;
  const required = new Set();
  const queue = [source];
  while (queue.length) {
    const file = queue.shift();
    const needs = dependencies(file);
    if (!needs) return names;
    for (const name of needs) {
      if (!names.includes(name) || required.has(name)) continue;
      required.add(name);
      queue.push(join(release, name));
    }
  }
  // Keep the small sherpa wrappers even when only one is linked directly; the
  // runtime may load the C++ wrapper dynamically. Filter large ONNX copies.
  return names.filter((name) => /sherpa-onnx/.test(name) || required.has(name));
}

export function stage({ env = process.env } = {}) {
  const release = join(targetDirectory(env), "release");
  const source = join(release, executable);
  if (!existsSync(source)) {
    console.error(`Missing ${source}. Run npm run engine:build first.`);
    return 1;
  }
  rmSync(destination, { recursive: true, force: true });
  mkdirSync(destination, { recursive: true });
  const names = readdirSync(release).filter((name) => candidate.test(name));
  const libraries = requiredLibraries(release, names, source);
  for (const name of [executable, ...libraries]) {
    const from = join(release, name);
    const to = join(destination, name);
    // Dereference symlinks so npm pack keeps required sonames as real files.
    copyFileSync(from, to);
    if (name === executable && platform !== "win32") chmodSync(to, 0o755);
  }
  writeFileSync(
    join(destination, "engine.json"),
    `${JSON.stringify({ platform, arch, executable, libraries }, null, 2)}\n`,
  );
  console.log(`Staged ${executable} and ${libraries.length} required libraries in ${destination}`);
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  process.exit(stage());
}
