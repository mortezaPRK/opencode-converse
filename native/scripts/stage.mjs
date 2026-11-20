// Install the locally built engine into the fixed app state folder (developer flow).
import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { join, resolve } from "node:path";

const native = fileURLToPath(new URL("../", import.meta.url));
const root = fileURLToPath(new URL("../../", import.meta.url));
const platform = process.platform;
const executable = platform === "win32" ? "opencode-converse.exe" : "opencode-converse";

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

export async function stage({ env = process.env, source: given } = {}) {
  const source = given ?? join(targetDirectory(env), "release", executable);
  if (!existsSync(source)) {
    console.error(`Missing ${source}. Run npm run engine:build first.`);
    return 1;
  }
  const dist = join(root, "dist", "client", "engine.js");
  if (!existsSync(dist)) {
    console.error(`Missing ${dist}. Run npm run build first.`);
    return 1;
  }
  const { installLocalEngine } = await import(pathToFileURL(dist).href);
  const installed = installLocalEngine(source, {
    packageRoot: root,
    env,
    ...(env.HOME || env.USERPROFILE ? { home: env.HOME ?? env.USERPROFILE } : {}),
  });
  console.log(`Installed ${executable} at ${installed}`);
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  process.exit(await stage());
}
