import type { InboxSource, Json, Thread } from "./types";

/** Rows of another workspace's threads under its sidebar entry, as in the TUI's sidebar. */
export const PEEK_ROWS = 6;
/** How often the sidebar re-reads the other workspaces while it is shown. */
export const WORKSPACE_REFRESH_MS = 15_000;

export interface WorkspaceEntry {
  /** Its id (`twr ws list --json`'s `id`), which selects it and keys what is kept of it: a
   * rename leaves it, and hubs, `hello` and the gateway's sources name it by it. */
  id: string;
  /** Its name as shown. */
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

/**
 * A `twr ws list --json` row's workspace id: what selects it (`-w`), what its hub reports as
 * `workspace` and what the gateway's sources name. An older twr's rows have no `id`: their
 * `name` is the id then.
 */
export const workspaceId = (value: Json) =>
  typeof value === "string" ? value : String(value?.id || value?.name || "");
/** A listing row's workspace name, as shown (its id until it is renamed). */
export const workspaceLabel = (value: Json) =>
  typeof value === "string" ? value : String(value?.name || value?.id || "");
/** The name shown for workspace `id`, from the listing; its id while the listing lacks it. */
export const labelOf = (listing: Json[], id: string) => {
  const row = listing.find((w) => workspaceId(w) === id);
  return row === undefined ? id : workspaceLabel(row) || id;
};
/**
 * The id of the workspace `selector` (a name or an id, as `-w` takes it) names in the
 * listing: the one so named, else the one with that id (names and ids share one namespace,
 * and a renamed workspace's original name stays its alias). `undefined`: none.
 */
export const resolveWorkspace = (listing: Json[], selector: string) => {
  const named = listing.find((w) => workspaceLabel(w) === selector);
  if (named !== undefined) return workspaceId(named);
  const byId = listing.find((w) => workspaceId(w) === selector);
  return byId === undefined ? undefined : workspaceId(byId);
};
/**
 * The id of the workspace to open by default: the listing's `default` row; with none (the
 * default workspace was removed and none chosen since) the first workspace listed, as a bare
 * `twr` opens; with no workspace at all, or from an older twr whose rows say nothing of a
 * default, `default`.
 */
export const defaultWorkspace = (listing: Json[]) => {
  const says = listing.some(
    (w) => typeof w === "object" && w && "default" in w,
  );
  const row =
    listing.find((w) => typeof w === "object" && w?.default === true) ??
    (says ? listing.find((w) => workspaceId(w)) : undefined);
  return row === undefined ? "default" : workspaceId(row);
};
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
 * Every workspace of the connected hub (`twr ws list` and the latest live scan), by id, the
 * default one first and then by name, each with what the sidebar shows of it. The current workspace's
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
  const listed = new Map(listing.map((w) => [workspaceId(w), w]));
  const first = defaultWorkspace(listing);
  const label = (id: string) => {
    const row = listed.get(id);
    return row === undefined ? id : workspaceLabel(row) || id;
  };
  const ids = [
    ...new Set([current, ...listed.keys(), ...scanned].filter(Boolean)),
  ].sort((a, b) =>
    a === first ? -1 : b === first ? 1 : label(a).localeCompare(label(b)),
  );
  return ids.map((id) => {
    const name = label(id);
    if (id === current && live)
      return {
        id,
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
    const source = sources.find((s) => s.hubId === hubId && s.workspace === id);
    const row = listed.get(id);
    const hub = typeof row === "object" && row ? row.hub : undefined;
    const running =
      source?.running === false || hub === "stopped"
        ? false
        : source && !source.error && !source.cached
          ? true
          : undefined;
    const { threads, more } = peekThreads(source);
    return {
      id,
      name,
      current: id === current,
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

/**
 * The workspace the desktop app opens as it starts: `TWR_WORKSPACE` when it is set (as `twr`
 * takes it); else the one it showed last, while the listing still has it; else the default
 * one, as `twr` without `-w` selects it (`default` when the listing could not be read).
 */
export function startWorkspace(
  env: string | undefined,
  saved: string | undefined,
  listing: Json[] | undefined,
): string {
  if (env) return env;
  if (!listing) return saved || "default";
  // The app saves the id it showed (an older one saved the name, which was the id then, as no
  // workspace could be renamed): by id alone, so a workspace renamed into that id since (the
  // one it showed removed) is never taken for it.
  const last = saved
    ? listing.find((w) => workspaceId(w) === saved)
    : undefined;
  return last !== undefined ? workspaceId(last) : defaultWorkspace(listing);
}
