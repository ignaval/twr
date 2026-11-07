import type { InboxSource } from "../shared/types";
import { workspaceSchema } from "./validation";
interface Reader {
  call(method: string, params?: any): Promise<any>;
  close(): void;
}
/** Concurrent refreshes share one scan, including its deadline and cleanup. */
export function coalesceInboxSources(refresh: () => Promise<InboxSource[]>) {
  let pending: Promise<InboxSource[]> | undefined;
  return () =>
    (pending ||= Promise.resolve()
      .then(refresh)
      .finally(() => {
        pending = undefined;
      }));
}
/** Short-lived read connections leave the active workspace and its terminals alone. */
export async function localInboxSources(
  workspaces: unknown,
  reader: (workspace: string) => Reader,
  budgetMs = 4000,
): Promise<InboxSource[]> {
  if (!Array.isArray(workspaces) || workspaces.length > 256)
    throw new Error("Invalid workspace listing");
  const listing = new Map<string, any>();
  for (const value of workspaces)
    listing.set(
      workspaceSchema.parse(typeof value === "string" ? value : value?.name),
      value,
    );
  const names = [...listing.keys()];
  const sources: InboxSource[] = new Array(names.length);
  let next = 0;
  const deadline = Date.now() + Math.max(0, budgetMs);
  async function worker() {
    while (next < names.length) {
      const index = next++;
      const workspace = names[index];
      const base: InboxSource = {
        hubId: "local",
        hubName: "This computer",
        workspace,
        attention: [],
        threads: [],
      };
      if (listing.get(workspace)?.hub === "stopped") {
        sources[index] = { ...base, running: false };
        continue;
      }
      if (Date.now() >= deadline) {
        sources[index] = {
          ...base,
          error: "Summary not checked before refresh deadline",
        };
        continue;
      }
      let connection: Reader | undefined;
      let identified = false;
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        connection = reader(workspace);
        const current = connection;
        const result = await Promise.race([
          (async () => {
            const info = await current.call("hub.info");
            if (info.workspace !== workspace)
              throw new Error("Socket serves another workspace");
            identified = true;
            const [attention, threads] = await Promise.all([
              current.call("attention.list", { all: true }),
              current.call("thread.list"),
            ]);
            if (!Array.isArray(attention) || !Array.isArray(threads))
              throw new Error("Invalid workspace summary");
            return { ...base, attention, threads };
          })(),
          new Promise<never>((_, reject) => {
            const remaining = Math.max(
              1,
              Math.min(3000, deadline - Date.now()),
            );
            timer = setTimeout(
              () =>
                reject(
                  new Error(
                    remaining === 3000
                      ? "Workspace did not answer within 3 seconds"
                      : "Workspace did not answer before the refresh deadline",
                  ),
                ),
              remaining,
            );
          }),
        ]);
        sources[index] = result;
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        const stopped =
          !identified &&
          /No such file|Connection refused|ECONNREFUSED|ENOENT|os error (2|61|111)\b/i.test(
            message,
          );
        sources[index] = {
          ...base,
          ...(stopped ? { running: false } : { error: message }),
        };
      } finally {
        clearTimeout(timer);
        connection?.close();
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(4, names.length) }, worker));
  return sources;
}
