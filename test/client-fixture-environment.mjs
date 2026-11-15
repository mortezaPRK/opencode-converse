import { join } from "node:path";
import { runtimeEnvironment } from "../dist/client/runtime.js";

/** Keep even a fixture's normal OS storage fallback inside its owned test root. */
export function fixtureEnvironment(directory, env = process.env) {
  const home = join(directory, "host-home");
  return {
    ...runtimeEnvironment(env),
    HOME: home,
    USERPROFILE: home,
    LOCALAPPDATA: join(home, "AppData", "Local"),
    XDG_DATA_HOME: join(home, ".local", "share"),
    OPENCODE_CONVERSE_DEV_DATA_DIR: directory,
  };
}
