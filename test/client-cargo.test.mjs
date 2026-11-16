import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { buildFixture } from "./client-cargo-fixture.mjs";

test("fixture executable comes from Cargo artifacts under custom directory and target configuration", () => {
  const executable = join(
    tmpdir(),
    "custom-artifacts",
    "host-triple",
    "debug",
    "opencode-converse",
  );
  const env = { CARGO_TARGET_DIR: "custom-artifacts", CARGO_BUILD_TARGET: "host-triple" };
  const spawn = (command, args, options) => {
    assert.equal(command, "cargo");
    assert.ok(args.includes("--message-format=json"));
    assert.deepEqual(options.env, { ...env, CARGO_PROFILE_DEV_DEBUG_ASSERTIONS: "true" });
    return {
      status: 0,
      stdout:
        JSON.stringify({
          reason: "compiler-artifact",
          target: { name: "opencode-converse", kind: ["bin"] },
          executable,
          profile: { debug_assertions: true },
        }) + "\n",
    };
  };
  assert.equal(buildFixture("/chosen", spawn, env), executable);
  assert.throws(
    () => buildFixture("/chosen", () => ({ status: 0, stdout: "" })),
    /executable just built/,
  );
});

test("fixture build forces debug assertions despite an inherited disabled dev profile", () => {
  const env = { CARGO_PROFILE_DEV_DEBUG_ASSERTIONS: "false", CARGO_TARGET_DIR: "custom-artifacts" };
  const executable = join(tmpdir(), "fixture", "opencode-converse");
  buildFixture(
    "/chosen",
    (_command, _args, options) => {
      assert.equal(options.env.CARGO_PROFILE_DEV_DEBUG_ASSERTIONS, "true");
      assert.equal(options.env.CARGO_TARGET_DIR, env.CARGO_TARGET_DIR);
      return {
        status: 0,
        stdout:
          JSON.stringify({
            reason: "compiler-artifact",
            target: { name: "opencode-converse", kind: ["bin"] },
            executable,
            profile: { debug_assertions: true },
          }) + "\n",
      };
    },
    env,
  );
  assert.equal(
    env.CARGO_PROFILE_DEV_DEBUG_ASSERTIONS,
    "false",
    "do not mutate the caller's environment",
  );
});

test("fixture build refuses a compiler artifact with disabled debug assertions before execution", () => {
  assert.throws(
    () =>
      buildFixture("/chosen", () => ({
        status: 0,
        stdout:
          JSON.stringify({
            reason: "compiler-artifact",
            target: { name: "opencode-converse", kind: ["bin"] },
            executable: join(tmpdir(), "fixture"),
            profile: { debug_assertions: false },
          }) + "\n",
      })),
    /debug-only data isolation/,
  );
});

test("fixture build preserves caller compiler flag precedence for the native compiled guard", () => {
  for (const env of [
    { RUSTFLAGS: "-Cdebug-assertions=no -C link-arg=existing" },
    { CARGO_ENCODED_RUSTFLAGS: "-Cdebug-assertions=no\u001f--cfg\u001ffixture_existing" },
    {
      RUSTFLAGS: "--cfg plain_ignored",
      CARGO_ENCODED_RUSTFLAGS: "-Cdebug-assertions=no\u001f--cfg\u001fencoded_wins",
    },
    {},
  ]) {
    const before = { ...env };
    buildFixture(
      "/selected",
      (_command, args, options) => {
        assert.equal(args[0], "build");
        assert.equal(args.includes("-Cdebug-assertions=yes"), false);
        assert.equal(options.env.RUSTFLAGS, env.RUSTFLAGS);
        assert.equal(options.env.CARGO_ENCODED_RUSTFLAGS, env.CARGO_ENCODED_RUSTFLAGS);
        return {
          status: 0,
          stdout:
            JSON.stringify({
              reason: "compiler-artifact",
              target: { name: "opencode-converse", kind: ["bin"] },
              executable: join(tmpdir(), "fixture"),
              profile: { debug_assertions: true },
            }) + "\n",
        };
      },
      env,
    );
    assert.deepEqual(env, before);
  }
});
