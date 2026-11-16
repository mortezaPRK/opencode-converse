import { rm } from "node:fs/promises";

export async function removeAfterClose(
  directory,
  owned,
  remove = (path) => rm(path, { recursive: true, force: true }),
  milliseconds = 1500,
) {
  let timer;
  try {
    if (owned?.child) {
      if (!owned.closed || typeof owned.closed.then !== "function")
        throw new Error("child close is untracked");
      await Promise.race([
        owned.closed,
        new Promise((_, reject) => {
          timer = setTimeout(() => reject(new Error("child close timeout")), milliseconds);
        }),
      ]);
    }
  } catch (error) {
    throw new Error(`Unconfirmed child closure; preserving storage: ${directory}`, {
      cause: error,
    });
  } finally {
    clearTimeout(timer);
  }
  await remove(directory);
}
