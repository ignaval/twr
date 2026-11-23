import type { EngineHub } from "../shared/types";
import { BUILD_ID, engineDifference, type Engine } from "../shared/engineHubs";
import { workspaceSchema } from "./validation";

interface Reader {
  call(method: string, params?: any, timeout?: number): Promise<any>;
  close(): void;
}

/**
 * Every running workspace hub in `workspaces` (a `twr ws list --json` listing) whose engine
 * differs from the app's, read from each hub's `hub.info` over `reader`. A hub that does not
 * answer within `timeoutMs`, or that serves another workspace, is left out: this is advice,
 * and the connection notice already covers a hub the app cannot reach.
 */
export async function engineHubs(
  workspaces: unknown,
  app: Engine,
  reader: (workspace: string) => Reader,
  timeoutMs = 3000,
): Promise<EngineHub[]> {
  if (!Array.isArray(workspaces) || workspaces.length > 256)
    throw new Error("Invalid workspace listing");
  const names: string[] = [];
  for (const value of workspaces) {
    const name = workspaceSchema.parse(
      typeof value === "string" ? value : value?.name,
    );
    // Only a hub the listing found running is asked; a bare name (no `hub`) is asked too.
    if (typeof value === "object" && value?.hub !== "running") continue;
    names.push(name);
  }
  const found = await Promise.all(
    names.map(async (workspace): Promise<EngineHub | undefined> => {
      let connection: Reader | undefined;
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        connection = reader(workspace);
        const current = connection;
        const info = await Promise.race([
          current.call("hub.info", {}, timeoutMs),
          new Promise<never>((_, reject) => {
            timer = setTimeout(() => reject(new Error("timed out")), timeoutMs);
          }),
        ]);
        if (!info || typeof info !== "object") return undefined;
        if (info.workspace !== undefined && info.workspace !== workspace)
          return undefined;
        const difference = engineDifference(app, info);
        if (!difference) return undefined;
        return {
          workspace,
          difference,
          version: String(info.version),
          ...(typeof info.build === "string" && BUILD_ID.test(info.build)
            ? { build: info.build }
            : {}),
        };
      } catch {
        return undefined;
      } finally {
        clearTimeout(timer);
        connection?.close();
      }
    }),
  );
  return found.filter((hub): hub is EngineHub => !!hub);
}
