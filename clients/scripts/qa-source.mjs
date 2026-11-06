import { execFileSync } from "node:child_process";

export function qaSource() {
  const options = {
    cwd: new URL("../..", import.meta.url),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  };
  try {
    return {
      sourceCommit: execFileSync("git", ["rev-parse", "HEAD"], options).trim(),
      sourceDirty: Boolean(
        execFileSync(
          "git",
          ["status", "--porcelain", "--untracked-files=no"],
          options,
        ).trim(),
      ),
    };
  } catch {
    return { sourceCommit: null, sourceDirty: null };
  }
}
