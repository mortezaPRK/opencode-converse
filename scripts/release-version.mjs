// Compute the version a release run publishes. Pure rule plus a small CLI.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

const PLAIN = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const positive = (value) => Number.isSafeInteger(value) && value > 0;

/**
 * If `v<current>` is not tagged yet the current version is released as it is;
 * otherwise the next patch is. Pull request builds append `-pr.<PR>.<run>`.
 */
export function releaseVersion({ current, tags, mode, pr, run }) {
  if (typeof current !== "string" || !PLAIN.test(current))
    throw new Error(`The repository version must be plain major.minor.patch, got ${current}`);
  if (!Array.isArray(tags)) throw new Error("tags must be a list");
  if (mode !== "stable" && mode !== "pr") throw new Error(`Unknown mode ${String(mode)}`);
  let next = current;
  if (tags.includes(`v${current}`)) {
    const [major, minor, patch] = current.split(".").map(Number);
    next = `${major}.${minor}.${patch + 1}`;
  }
  if (mode === "stable") return next;
  if (!positive(pr)) throw new Error("A pull request number is required");
  if (!positive(run)) throw new Error("A run number is required");
  return `${next}-pr.${pr}.${run}`;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const args = Object.fromEntries(
    process.argv.slice(2).map((arg) => {
      const [key, ...rest] = arg.replace(/^--/, "").split("=");
      return [key, rest.join("=")];
    }),
  );
  const root = fileURLToPath(new URL("../", import.meta.url));
  const current = JSON.parse(readFileSync(`${root}package.json`, "utf8")).version;
  const tags = execFileSync("git", ["tag", "--list", "v*"], { cwd: root, encoding: "utf8" })
    .split("\n")
    .filter(Boolean);
  try {
    console.log(
      releaseVersion({
        current,
        tags,
        mode: args.mode,
        pr: args.pr ? Number(args.pr) : undefined,
        run: args.run ? Number(args.run) : undefined,
      }),
    );
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }
}
