import type { EngineHub } from "../shared/types";
import { BUILD_ID, engineDifference, type Engine } from "../shared/engineHubs";
import { workspaceSchema } from "./validation";
import { workspaceId, workspaceLabel } from "../shared/workspaceTree";

/**
 * Every running workspace hub in `workspaces` (a `twr ws list --json` listing) whose engine
 * differs from the app's. Each running row carries its hub's `version` and `build`, as its
 * `hub.info` gave them to the listing, so this opens no connection of its own: a refresh's
 * read-only bridges cost a process and a peer check each (`desktop/inbox.ts`). A row without
 * a version (a hub that did not answer, or a listing from a twr before it had one) is left
 * out: this is advice, and the connection notice covers a hub the app cannot reach.
 */
export function engineHubs(workspaces: unknown, app: Engine): EngineHub[] {
  if (!Array.isArray(workspaces) || workspaces.length > 256)
    throw new Error("Invalid workspace listing");
  const hubs: EngineHub[] = [];
  for (const row of workspaces) {
    if (!row || typeof row !== "object" || row.hub !== "running") continue;
    // Keyed by id, as the app's workspace is; named as shown (a renamed one's new name).
    const workspace = workspaceSchema.parse(workspaceId(row));
    const name = workspaceSchema.safeParse(workspaceLabel(row));
    const difference = engineDifference(app, row);
    if (!difference) continue;
    hubs.push({
      workspace,
      ...(name.success && name.data !== workspace ? { name: name.data } : {}),
      difference,
      version: String(row.version),
      ...(typeof row.build === "string" && BUILD_ID.test(row.build)
        ? { build: row.build }
        : {}),
    });
  }
  return hubs;
}
