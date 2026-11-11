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
/** A CLI from before `twr ws list --detail` refuses the flag (clap's usage error). */
function refusesDetail(error: unknown) {
  const stderr = String((error as { stderr?: unknown })?.stderr ?? "");
  return /unexpected argument '--detail'/.test(stderr);
}
/** A row of an earlier listing, without what its hub listed then: that is read again. */
function withoutDetail(row: unknown) {
  if (!row || typeof row !== "object") return row;
  const rest: Record<string, unknown> = { ...row };
  delete rest.detail;
  delete rest.detail_error;
  return rest;
}
/**
 * The workspace list for a refresh, from `twr ws list --json --detail`: each running hub's
 * threads and requests come with it, read once, so the refresh needs no connection of its
 * own. A CLI without `--detail` is listed without it, and its workspaces are read one by one
 * as before. The listing gets one overall budget (one busy hub can make it slow), and a
 * listing that fails or runs out of time falls back to the last one that worked, without what
 * the hubs listed then.
 */
export class WorkspaceListing {
  private last?: unknown[];
  private detail = true;
  constructor(
    private readonly list: (
      timeoutMs: number,
      detail: boolean,
    ) => Promise<unknown>,
    private readonly budgetMs = 5000,
  ) {}
  private async read(): Promise<unknown> {
    if (!this.detail) return this.list(this.budgetMs, false);
    try {
      return await this.list(this.budgetMs, true);
    } catch (error) {
      if (!refusesDetail(error)) throw error;
      this.detail = false;
      return this.list(this.budgetMs, false);
    }
  }
  async get(): Promise<unknown[]> {
    try {
      const value: any = await this.read();
      const workspaces = Array.isArray(value) ? value : value?.workspaces;
      if (!Array.isArray(workspaces))
        throw new Error("Invalid workspace listing");
      this.last = workspaces;
      return workspaces;
    } catch (error) {
      if (this.last) return this.last.map(withoutDetail);
      throw error;
    }
  }
}
/**
 * One source per listed workspace. A row with `detail` (or `detail_error`) was read by the
 * listing itself; any other running workspace (an older CLI's listing, or the last good one)
 * is read over a short-lived read-only connection, which leaves the active workspace and its
 * terminals alone.
 */
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
  const base = (workspace: string): InboxSource => ({
    hubId: "local",
    hubName: "This computer",
    workspace,
    attention: [],
    threads: [],
  });
  const unread: number[] = [];
  names.forEach((workspace, index) => {
    const row = listing.get(workspace);
    if (row?.hub === "stopped")
      sources[index] = { ...base(workspace), running: false };
    else if (row?.detail !== undefined) {
      const { attention, threads } = row.detail ?? {};
      sources[index] =
        Array.isArray(attention) && Array.isArray(threads)
          ? { ...base(workspace), attention, threads }
          : { ...base(workspace), error: "Invalid workspace summary" };
    } else if (typeof row?.detail_error === "string")
      sources[index] = { ...base(workspace), error: row.detail_error };
    else unread.push(index);
  });
  let next = 0;
  const deadline = Date.now() + Math.max(0, budgetMs);
  async function worker() {
    while (next < unread.length) {
      const index = unread[next++];
      const workspace = names[index];
      if (Date.now() >= deadline) {
        sources[index] = {
          ...base(workspace),
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
            return { ...base(workspace), attention, threads };
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
          ...base(workspace),
          ...(stopped ? { running: false } : { error: message }),
        };
      } finally {
        clearTimeout(timer);
        connection?.close();
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(4, unread.length) }, worker));
  return sources;
}
