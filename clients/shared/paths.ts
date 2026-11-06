import type { Thread } from "./types";
/** Remote worktree paths belong to their runner, not this desktop machine. */
export function localWorkingDirectory(
  thread: Thread | undefined,
  home: string,
): string {
  return thread?.box_name && thread.box_name !== "local"
    ? home
    : thread?.worktree || thread?.repo || home;
}
