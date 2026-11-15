// Source-build entry point. sherpa-rs-sys 0.6.8 copies shared libraries into
// target/<profile>/deps but does not create that directory on recent Cargo.
import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { basename, extname, join, resolve } from "node:path";

const native = fileURLToPath(new URL("../", import.meta.url));
export function build({ spawn = spawnSync, env: inputEnv = process.env } = {}) {
  const env = { ...inputEnv };
  if (process.platform === "darwin") env.MACOSX_DEPLOYMENT_TARGET ??= "13.4";
  // Ask Cargo to resolve target-dir, including local Cargo configuration and relative
  // CARGO_TARGET_DIR values, rather than guessing from the manifest location.
  const metadata = spawn(
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
    return metadata.status ?? 1;
  }
  const target = JSON.parse(metadata.stdout).target_directory;
  const buildTarget = env.CARGO_BUILD_TARGET;
  const targetName = buildTarget?.endsWith(".json")
    ? basename(buildTarget, extname(buildTarget))
    : buildTarget;
  for (const profile of ["debug", "release"]) {
    mkdirSync(join(target, profile, "deps"), { recursive: true });
    if (targetName) mkdirSync(join(target, targetName, profile, "deps"), { recursive: true });
  }
  const result = spawn(
    "cargo",
    [
      "build",
      "--locked",
      "--manifest-path",
      join(native, "Cargo.toml"),
      "--release",
      "-p",
      "opencode-converse",
      "--features",
      "native-audio",
    ],
    { stdio: "inherit", env },
  );
  if (result.error) console.error(result.error.message);
  return result.status ?? 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  process.exit(build());
}
