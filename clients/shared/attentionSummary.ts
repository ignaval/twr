import type {
  Attention,
  AttentionSummary,
  AttentionSummaryItem,
  InboxSource,
  LoopEndSummary,
  Thread,
} from "./types";
import { endedWords, requestLine, type LastLoop } from "./loops";

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

/** The thread of the hub's own questions (twr-core `HUB_THREAD`). */
const HUB_THREAD = "hub";

/**
 * Which of the hub's cards an item is: a question on the hub's thread that the hub marks
 * `proposal` (twr-core `is_confirm_card`), a delete card when it offers Delete (its options
 * are `Delete`/`Cancel`; a confirm card's are `Confirm`/`Cancel`); a loop's setup card or a
 * stuck loop's item, which the hub marks on the loop's thread (`AttentionItem::loop_setup`,
 * `loop_stuck`). Every one of them needs the user, whatever its reason.
 */
export function cardOf(
  item: Pick<
    Attention,
    "thread" | "proposal" | "options" | "loop_setup" | "loop_stuck"
  >,
): AttentionSummaryItem["card"] {
  if (item.loop_setup === true) return "loop_setup";
  if (item.loop_stuck === true) return "loop_stuck";
  if (item.thread !== HUB_THREAD || item.proposal !== true) return undefined;
  return Array.isArray(item.options) && item.options.includes("Delete")
    ? "delete"
    : "confirm";
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
  loopRounds,
}: {
  current: string;
  hubId: string;
  /** The shown workspace's view; none while it connects, when its last read stands in. */
  live?: { threads: Thread[]; attention: Attention[] };
  liveStream?: string;
  sources: InboxSource[];
  max?: number;
  /** Rounds each loop ran, by loop id, as seen while it ran (an ended loop's view keeps no
   * rounds): a loop's end says `after N rounds` when it is known. */
  loopRounds?: ReadonlyMap<string, number>;
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
  const loops: LoopEndSummary[] = [];
  let count = 0;
  for (const list of lists) {
    for (const thread of list.threads) {
      const last = thread?.last_loop as LastLoop | undefined | null;
      if (!last?.id || typeof thread.id !== "string" || loops.length >= max)
        continue;
      loops.push({
        workspace: list.name,
        thread: thread.id,
        title: String(thread.name || thread.id).slice(0, 200),
        id: String(last.id).slice(0, 200),
        words: endedWords(last, loopRounds?.get(last.id)).slice(0, 200),
      });
    }
    const names = new Map(list.threads.map((t) => [t?.id, t?.name]));
    for (const item of list.attention) {
      if (!item || typeof item.attn_id !== "string") continue;
      count++;
      if (items.length >= max) continue;
      const card = cardOf(item);
      items.push({
        workspace: list.name,
        thread: item.thread,
        // The hub's own questions are on its thread `hub`; they are twr's, as in its [notify].
        title: (
          names.get(item.thread) ||
          (item.thread === HUB_THREAD ? "twr" : item.thread) ||
          "twr"
        ).slice(0, 200),
        // A loop's setup card: its first line says what it asks; the rest are its rows
        // (an implementation loop's held task among them).
        summary: (requestLine(item) || item.reason || "").slice(0, 300),
        id: item.attn_id,
        revision: Number.isSafeInteger(item.revision) ? item.revision : 0,
        reason: String(item.reason || "").slice(0, 40),
        ...(card ? { card } : {}),
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
    ...(loops.length ? { loops } : {}),
  };
}
