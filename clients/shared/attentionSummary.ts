import type {
  Attention,
  AttentionSummary,
  AttentionSummaryItem,
  InboxSource,
  Thread,
} from "./types";

/** The most items a summary lists; `count` still counts every one. */
export const SUMMARY_MAX_ITEMS = 200;

/** A workspace read is current: a live scan that answered, not a saved or failed one. */
export function currentSource(source: InboxSource) {
  return (
    !source.cached &&
    !source.error &&
    !source.scanError &&
    !source.provisional &&
    source.running !== false
  );
}

const order = (a: string, b: string) =>
  a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b);

/**
 * What waits on the user in every workspace of the connected hub (`hubId`), for the
 * desktop's tray, Dock badge and notifications. The shown workspace's items come from its
 * live view, the others' from the latest workspace reads (`sources`, as the sidebar shows
 * them). `liveStream` names the live view's connection while it is current (see
 * {@link AttentionSummary.workspaces}); the reads share one stream per hub.
 */
export function attentionSummary({
  current,
  hubId,
  live,
  liveStream,
  sources,
  max = SUMMARY_MAX_ITEMS,
}: {
  current: string;
  hubId: string;
  /** The shown workspace's view; none while it connects, when its last read stands in. */
  live?: { threads: Thread[]; attention: Attention[] };
  liveStream?: string;
  sources: InboxSource[];
  max?: number;
}): AttentionSummary {
  const lists: {
    name: string;
    stream?: string;
    threads: Thread[];
    attention: Attention[];
  }[] = [];
  const seen = new Set<string>();
  const read = (source: InboxSource) => ({
    name: source.workspace,
    stream: currentSource(source) ? `read:${hubId}` : undefined,
    threads: Array.isArray(source.threads) ? source.threads : [],
    attention: Array.isArray(source.attention) ? source.attention : [],
  });
  const own = sources.filter((source) => source.hubId === hubId);
  if (live) lists.push({ name: current, stream: liveStream, ...live });
  else {
    const source = own.find((s) => s.workspace === current);
    if (source) lists.push({ ...read(source), stream: undefined });
  }
  seen.add(current);
  for (const source of [...own].sort((a, b) =>
    order(a.workspace, b.workspace),
  )) {
    if (seen.has(source.workspace)) continue;
    seen.add(source.workspace);
    lists.push(read(source));
  }
  const items: AttentionSummaryItem[] = [];
  let count = 0;
  for (const list of lists) {
    const names = new Map(list.threads.map((t) => [t?.id, t?.name]));
    for (const item of list.attention) {
      if (!item || typeof item.attn_id !== "string") continue;
      count++;
      if (items.length >= max) continue;
      items.push({
        workspace: list.name,
        thread: item.thread,
        title: (names.get(item.thread) || item.thread || "twr").slice(0, 200),
        summary: (item.summary || item.reason || "").slice(0, 300),
        id: item.attn_id,
        revision: Number.isSafeInteger(item.revision) ? item.revision : 0,
        reason: String(item.reason || "").slice(0, 40),
        ...(item.incomplete ? { incomplete: true } : {}),
      });
    }
  }
  return {
    count,
    items,
    workspaces: lists.map(({ name, stream }) =>
      stream ? { name, stream } : { name },
    ),
  };
}
