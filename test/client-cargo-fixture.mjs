import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { isAbsolute } from "node:path";

export function buildFixture(root, spawn = spawnSync, env = process.env) {
  // The dev-only data root must never be disabled by an inherited profile.
  const buildEnv = { ...env, CARGO_PROFILE_DEV_DEBUG_ASSERTIONS: "true" };
  const built = spawn(
    "cargo",
    [
      "build",
      "--locked",
      "--manifest-path",
      "native/Cargo.toml",
      "-p",
      "opencode-converse",
      "--message-format=json",
    ],
    { cwd: root, encoding: "utf8", timeout: 180_000, env: buildEnv },
  );
  assert.equal(built.status, 0, built.stderr || built.error?.message);
  const artifacts = built.stdout
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line))
    .filter(
      (message) =>
        message.reason === "compiler-artifact" &&
        message.target?.name === "opencode-converse" &&
        message.target.kind.includes("bin") &&
        message.executable,
    );
  assert.equal(artifacts.length, 1, "Cargo must identify the executable just built");
  assert.equal(
    artifacts[0].profile?.debug_assertions,
    true,
    "fixture executable must retain debug-only data isolation",
  );
  const executable = artifacts[0].executable;
  assert.equal(isAbsolute(executable), true, "Cargo must report an absolute executable path");
  return executable;
}
