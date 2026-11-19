import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join, isAbsolute } from "node:path";
import test from "node:test";
import { fixtureEnvironment } from "./client-fixture-environment.mjs";

test("fixture OS home and data fallbacks stay isolated without passing compiler flags or credentials", () => {
  const directory = join(tmpdir(), "fixture-owned-root");
  const env = fixtureEnvironment(directory, {
    HOME: "/user-home",
    USERPROFILE: "/user-profile",
    LOCALAPPDATA: "/user-data",
    XDG_DATA_HOME: "/user-xdg",
    RUSTFLAGS: "-Cdebug-assertions=no",
    CARGO_ENCODED_RUSTFLAGS: "-Cdebug-assertions=no",
    OPENCODE_SERVER_PASSWORD: "secret",
    OPENAI_API_KEY: "secret",
  });
  for (const key of ["HOME", "USERPROFILE", "LOCALAPPDATA", "XDG_DATA_HOME"]) {
    assert.equal(isAbsolute(env[key]), true);
    assert.ok(env[key].startsWith(directory));
  }
  for (const key of [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "OPENCODE_SERVER_PASSWORD",
    "OPENAI_API_KEY",
  ])
    assert.equal(env[key], undefined);
});
