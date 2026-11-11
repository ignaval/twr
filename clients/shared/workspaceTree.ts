import type { InboxSource, Json, Thread } from "./types";

/** Rows of another workspace's threads under its sidebar entry, as in the TUI's sidebar. */
export const PEEK_ROWS = 6;
/** How often the sidebar re-reads the other workspaces while it is shown. */
export const WORKSPACE_REFRESH_MS = 15_000;

export interface WorkspaceEntry {
  name: string;
  current: boolean;
  /** `false` when the workspace's hub is known to be stopped. */
  running?: boolean;
  /** Requests waiting on you. */
  waiting: number;
  /** Worker threads at work (running or in the background). */
  working: number;
  /** Other workspaces' threads, as their hub last reported them (see {@link peekThreads}). */
  threads: Thread[];
  /** Threads beyond {@link PEEK_ROWS} that are not listed. */
  more: number;
  /** The summary is saved or failed to refresh rather than live. */
  stale: boolean;
}

const nameOf = (value: Json) =>
  typeof value === "string" ? value : String(value?.name || "");
const working = (t: Thread) =>
  t.status === "running" || t.status === "background";

/**
 * Another workspace's threads as its sidebar entry lists them: no orchestrator and no parked
 * threads; those waiting on you first, then failed ones, those at work and the rest, keeping
 * the hub's order within each; at most `max` rows.
 */
export function peekThreads(
  source: Pick<InboxSource, "threads" | "attention"> | undefined,
  max = PEEK_ROWS,
): { threads: Thread[]; more: number } {
  if (!source) return { threads: [], more: 0 };
  const asking = new Set(source.attention.map((a) => a.thread));
  const rank = (t: Thread) =>
    asking.has(t.id) || t.status === "waiting"
      ? 0
      : t.status === "error" || t.status === "failed"
        ? 1
        : working(t)
          ? 2
          : 3;
  const rows = source.threads
    .filter((t) => t && !t.is_orchestrator && t.status !== "parked")
    .map((t, index) => ({ t, index }))
    .sort((a, b) => rank(a.t) - rank(b.t) || a.index - b.index)
    .map(({ t }) => t);
  return rows.length > max
    ? { threads: rows.slice(0, max - 1), more: rows.length - (max - 1) }
    : { threads: rows, more: 0 };
}

/**
 * Every workspace of the connected hub (`twr ws list` and the latest live scan), `default`
 * first and then by name, each with what the sidebar shows of it. The current workspace's
 * counts come from its live view; the others from the latest workspace scan (`sources`,
 * filtered to `hubId`), falling back to `twr ws list`.
 */
export function workspaceEntries(
  listing: Json[],
  current: string,
  hubId: string,
  sources: InboxSource[],
  /** The current workspace's live view; none while it connects, when it is shown as scanned. */
  live: { threads: Thread[]; attention: { thread: string }[] } | undefined,
): WorkspaceEntry[] {
  // A live scan also names workspaces made since the listing was read; a saved summary alone
  // may be of one since removed.
  const scanned = sources
    .filter((s) => s.hubId === hubId && !s.cached && !s.scanError)
    .map((s) => s.workspace);
  const names = [
    ...new Set([current, ...listing.map(nameOf), ...scanned].filter(Boolean)),
  ].sort((a, b) =>
    a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b),
  );
  const listed = new Map(listing.map((w) => [nameOf(w), w]));
  return names.map((name) => {
    if (name === current && live)
      return {
        name,
        current: true,
        running: true,
        waiting: live.attention.length,
        working: live.threads.filter((t) => !t.is_orchestrator && working(t))
          .length,
        threads: [],
        more: 0,
        stale: false,
      };
    const source = sources.find(
      (s) => s.hubId === hubId && s.workspace === name,
    );
    const row = listed.get(name);
    const hub = typeof row === "object" && row ? row.hub : undefined;
    const running =
      source?.running === false || hub === "stopped"
        ? false
        : source && !source.error && !source.cached
          ? true
          : undefined;
    const { threads, more } = peekThreads(source);
    return {
      name,
      current: name === current,
      running,
      waiting: source
        ? source.attention.length
        : Number(typeof row === "object" && row ? row.waiting : 0) || 0,
      working: source
        ? source.threads.filter((t) => t && !t.is_orchestrator && working(t))
            .length
        : Number(typeof row === "object" && row ? row.running : 0) || 0,
      threads,
      more,
      stale: !!source && (!!source.cached || !!source.error),
    };
  });
}
