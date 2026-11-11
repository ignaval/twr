import type { Attention, DesktopAPI, InboxSource, Thread } from "./types";

export const INBOX_CACHE_TTL = 7 * 24 * 60 * 60 * 1000;
const MAX_HUBS = 64;
const MAX_RECORD_BYTES = 6 * 1024 * 1024;
const MAX_WORKSPACES = 32;
const MAX_SOURCES = 128;
const MAX_ROWS = 200;
const READ_CONCURRENCY = 3;
const CACHE_TIMEOUT = 1500;
const LIVE_TIMEOUT = 6500;
export interface InboxHub {
  id: string;
  name: string;
}
interface Options {
  live(): Promise<InboxSource[]>;
  listHubs(): Promise<{ hubs: InboxHub[] }>;
  secureRead?: DesktopAPI["secureRead"];
  currentHub?: InboxHub;
  includeLocal?: boolean;
  now?: number;
}
type Settled<T> =
  { kind: "value"; value: T } | { kind: "error" } | { kind: "timeout" };
// Keep native reads bounded across refreshes and host switches, including calls that never
// settle. A repeated refresh shares an existing read instead of opening another keyring call.
const reads = new WeakMap<
  NonNullable<DesktopAPI["secureRead"]>,
  Map<string, Promise<string | null>>
>();
function readCache(reader: NonNullable<DesktopAPI["secureRead"]>, key: string) {
  let pool = reads.get(reader);
  if (!pool) {
    pool = new Map();
    reads.set(reader, pool);
  }
  const existing = pool.get(key);
  if (existing) return existing;
  if (pool.size >= READ_CONCURRENCY) return;
  const pending = Promise.resolve()
    .then(() => reader(key))
    .finally(() => {
      pool!.delete(key);
    });
  pool.set(key, pending);
  return pending;
}
/** Timed-out cache workers stop: unresolved keyring calls still occupy their read slot. */
async function bounded<T>(
  promise: Promise<T>,
  milliseconds: number,
): Promise<Settled<T>> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      promise.then(
        (value): Settled<T> => ({ kind: "value", value }),
        (): Settled<T> => ({ kind: "error" }),
      ),
      new Promise<Settled<T>>((resolve) => {
        timer = setTimeout(() => resolve({ kind: "timeout" }), milliseconds);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
function text(value: unknown, limit: number): string {
  return typeof value === "string" ? value.slice(0, limit) : "";
}
function identifier(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= 256 &&
    !/[\x00-\x1f\x7f]/.test(value)
  );
}
function metadata(value: unknown): InboxHub | undefined {
  if (!value || typeof value !== "object") return;
  const hub = value as Partial<InboxHub>;
  if (typeof hub.id !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(hub.id))
    return;
  return { id: hub.id, name: text(hub.name, 80) || "Saved hub" };
}
function threadSummary(raw: unknown): Thread | undefined {
  if (!raw || typeof raw !== "object") return;
  const row = raw as Thread;
  if (!identifier(row.id)) return;
  return {
    id: row.id,
    name: text(row.name, 160) || row.id,
    runtime: text(row.runtime, 80),
    box_name: text(row.box_name, 80),
    mode: text(row.mode, 40),
    status: text(row.status, 40),
    accent:
      typeof row.accent === "string" && /^#[a-fA-F0-9]{6}$/.test(row.accent)
        ? row.accent
        : "#808080",
    is_orchestrator: row.is_orchestrator === true,
  };
}
function attentionSummary(raw: unknown): Attention | undefined {
  if (!raw || typeof raw !== "object") return;
  const row = raw as Attention;
  if (!identifier(row.attn_id) || !identifier(row.thread)) return;
  return {
    attn_id: row.attn_id,
    thread: row.thread,
    revision:
      Number.isSafeInteger(row.revision) && row.revision >= 0
        ? row.revision
        : 0,
    reason: text(row.reason, 40),
    summary: text(row.summary, 2048),
    // A saved summary carries no response choices, context payload, or mutable controls.
    options: [],
    incomplete: true,
  };
}
/** Read only the display summary from OfflineJournal's encrypted record. */
export function cachedInboxSources(
  value: string | null,
  hub: InboxHub,
  now = Date.now(),
): InboxSource[] {
  if (
    typeof value !== "string" ||
    value.length > MAX_RECORD_BYTES ||
    new TextEncoder().encode(value).byteLength > MAX_RECORD_BYTES
  )
    return [];
  let record: any;
  try {
    record = JSON.parse(value);
  } catch {
    return [];
  }
  if (
    record?.version !== 1 ||
    !record.workspaces ||
    typeof record.workspaces !== "object" ||
    Array.isArray(record.workspaces)
  )
    return [];
  const names = Object.keys(record.workspaces);
  if (names.length > 256) return [];
  return names
    .filter((workspace) =>
      /^(?:[a-z0-9]|[a-z0-9][a-z0-9-]{0,30}[a-z0-9])$/.test(workspace),
    )
    .map((workspace) => ({ workspace, cached: record.workspaces[workspace] }))
    .filter(
      ({ cached }) =>
        Number.isFinite(cached?.savedAt) &&
        cached.savedAt > 0 &&
        cached.savedAt <= now &&
        now - cached.savedAt < INBOX_CACHE_TTL &&
        Array.isArray(cached.view?.threads) &&
        Array.isArray(cached.view?.attention),
    )
    .sort((a, b) => b.cached.savedAt - a.cached.savedAt)
    .slice(0, MAX_WORKSPACES)
    .map(({ workspace, cached }) => ({
      hubId: hub.id,
      hubName: hub.name,
      workspace,
      cached: true,
      updatedAt: cached.savedAt,
      threads: cached.view.threads
        .slice(0, MAX_ROWS)
        .map(threadSummary)
        .filter((row: Thread | undefined): row is Thread => !!row),
      attention: cached.view.attention
        .slice(0, MAX_ROWS)
        .map(attentionSummary)
        .filter((row: Attention | undefined): row is Attention => !!row),
    }));
}
/** Live data wins within its own hub/workspace; other saved hubs remain display-only. */
export async function aggregateInboxSources(
  options: Options,
): Promise<InboxSource[]> {
  const now = options.now ?? Date.now();
  const livePending = bounded(
    Promise.resolve().then(options.live),
    LIVE_TIMEOUT,
  );
  const listing = await bounded(
    Promise.resolve().then(options.listHubs),
    CACHE_TIMEOUT,
  );
  const hubs = new Map<string, InboxHub>();
  if (listing.kind === "value" && Array.isArray(listing.value?.hubs)) {
    for (const value of listing.value.hubs.slice(0, MAX_HUBS)) {
      const hub = metadata(value);
      if (hub) hubs.set(hub.id, hub);
    }
  }
  const current = metadata(options.currentHub);
  if (current) hubs.set(current.id, current);
  if (options.includeLocal)
    hubs.set("local", { id: "local", name: "This computer" });
  const cache: InboxSource[] = [];
  let cacheReadFailed = false;
  if (options.secureRead) {
    const entries = [...hubs.values()];
    let next = 0;
    async function worker() {
      while (next < entries.length) {
        const hub = entries[next++];
        const pending = readCache(options.secureRead!, `offline:${hub.id}`);
        if (!pending) {
          cacheReadFailed = true;
          return;
        }
        const result = await bounded(pending, CACHE_TIMEOUT);
        if (result.kind === "timeout") {
          cacheReadFailed = true;
          return;
        }
        if (result.kind === "error") cacheReadFailed = true;
        if (result.kind === "value")
          cache.push(...cachedInboxSources(result.value, hub, now));
      }
    }
    await Promise.all(
      Array.from(
        { length: Math.min(READ_CONCURRENCY, entries.length) },
        worker,
      ),
    );
  }
  const sources = new Map<string, InboxSource>();
  const key = (source: InboxSource) =>
    JSON.stringify([source.hubId, source.workspace]);
  for (const source of cache
    .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))
    .slice(0, MAX_SOURCES))
    sources.set(key(source), source);
  const live = await livePending;
  if (live.kind === "value" && Array.isArray(live.value)) {
    for (const source of live.value.slice(0, 256)) {
      if (
        !source ||
        !identifier(source.hubId) ||
        !identifier(source.workspace) ||
        !Array.isArray(source.attention) ||
        !Array.isArray(source.threads)
      )
        continue;
      // A failed workspace read must not erase its last useful saved summary.
      const previous = sources.get(key(source));
      if ((source.error || source.running === false) && previous) {
        sources.set(key(source), {
          ...previous,
          error: source.error,
          running: source.running,
        });
        continue;
      }
      sources.set(key(source), source);
    }
  }
  const result = [...sources.values()];
  if (
    live.kind !== "value" ||
    !Array.isArray(live.value) ||
    listing.kind !== "value" ||
    cacheReadFailed
  )
    result.push({
      hubId: "__inbox_scan__",
      hubName: "Workspace scan",
      workspace: "",
      attention: [],
      threads: [],
      scanError: true,
      error:
        live.kind !== "value" || !Array.isArray(live.value)
          ? "The workspace scan could not finish. Some requests may be missing. Refresh to try again."
          : "Saved hub summaries could not be read. Unlock encrypted storage or refresh to try again.",
    });
  return result;
}
export function withInboxCache(
  api: DesktopAPI,
  listHubs: Options["listHubs"],
): DesktopAPI {
  let pending: Promise<InboxSource[]> | undefined;
  return {
    ...api,
    inboxSources: () =>
      (pending ||= aggregateInboxSources({
        live: () => api.inboxSources?.() ?? Promise.resolve([]),
        listHubs,
        secureRead: api.secureRead,
        includeLocal: true,
      }).finally(() => {
        pending = undefined;
      })),
  };
}
