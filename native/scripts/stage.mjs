// Stage the release engine executable for npm packaging.
import { chmodSync, copyFileSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { join, resolve } from "node:path";

const native = fileURLToPath(new URL("../", import.meta.url));
const root = fileURLToPath(new URL("../../", import.meta.url));
const platform = process.platform;
const arch = process.arch;
const executable = platform === "win32" ? "opencode-converse.exe" : "opencode-converse";
const destination = join(root, "assets", "bin", `${platform}-${arch}`);

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

export function stage({ env = process.env } = {}) {
  const release = join(targetDirectory(env), "release");
  const source = join(release, executable);
  if (!existsSync(source)) {
    console.error(`Missing ${source}. Run npm run engine:build first.`);
    return 1;
  }
  rmSync(destination, { recursive: true, force: true });
  mkdirSync(destination, { recursive: true });
  // The engine is one self-contained executable: ONNX Runtime is linked statically and
  // Phonon (ptts) is pure Rust, so there are no shared libraries to stage.
  const to = join(destination, executable);
  copyFileSync(source, to);
  if (platform !== "win32") chmodSync(to, 0o755);
  writeFileSync(
    join(destination, "engine.json"),
    `${JSON.stringify({ platform, arch, executable }, null, 2)}\n`,
  );
  console.log(`Staged ${executable} in ${destination}`);
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  process.exit(stage());
}
