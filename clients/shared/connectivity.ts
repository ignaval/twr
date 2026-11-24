import type { Attention } from "./types";

/** The thread of the hub's own items (twr-core `HUB_THREAD`). */
const HUB_THREAD = "hub";

/**
 * Whether the hub raised `item` about its machine's internet connection (twr-hub `hub::net`,
 * twr-core `AttentionItem::offline_since`): its own notice on the hub's thread, or a thread's
 * stall while the connection was down.
 */
export function isOffline(item: Pick<Attention, "offline_since">): boolean {
  return typeof item.offline_since === "string" && item.offline_since !== "";
}

/** The hub's open notice that its machine has no internet connection, if any. */
export function hubOffline(
  attention: readonly Attention[],
): Attention | undefined {
  return attention.find((a) => a.thread === HUB_THREAD && isOffline(a));
}

/** `iso` as this device shows the time: `14:02`, with the day when not today (`Mon 14:02`). */
export function clockTime(iso: string, now = new Date()): string | undefined {
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return undefined;
  const hhmm = `${String(t.getHours()).padStart(2, "0")}:${String(t.getMinutes()).padStart(2, "0")}`;
  if (t.toDateString() === now.toDateString()) return hhmm;
  return `${t.toLocaleDateString(undefined, { weekday: "short" })} ${hhmm}`;
}

/**
 * The status strip's words for the hub's notice `item`: since when, and what it means for
 * the threads (`waiting`: how many of them have a stall marked offline).
 */
export function offlineNoticeText(
  item: Pick<Attention, "offline_since">,
  waiting = 0,
  now = new Date(),
): string {
  const since = item.offline_since
    ? clockTime(item.offline_since, now)
    : undefined;
  const threads =
    waiting === 1
      ? " 1 thread is waiting on it."
      : waiting > 1
        ? ` ${waiting} threads are waiting on it.`
        : "";
  return (
    `No internet connection on the hub's machine${since ? ` since ${since}` : ""}: ` +
    "agents there can't reach their model APIs. twr holds stall alerts and automatic " +
    `retries until it's back.${threads}`
  );
}

/** How many threads have a stall the hub marked offline. */
export function offlineThreads(attention: readonly Attention[]): number {
  return new Set(
    attention
      .filter((a) => a.thread !== HUB_THREAD && isOffline(a))
      .map((a) => a.thread),
  ).size;
}

/** The heading of an attention card: what kind of item it is. */
export function attentionKindLabel(
  item: Pick<Attention, "reason" | "loop_stuck" | "offline_since"> &
    Partial<Pick<Attention, "thread" | "proposal" | "options" | "delete">>,
): string {
  if (item.loop_stuck) return "Loop stuck";
  if (isOffline(item)) return "No internet";
  // A delete card is the hub asking to confirm what goes, not a question to answer in words.
  if (
    item.delete ||
    (item.thread === "hub" && item.proposal && item.options?.includes("Delete"))
  )
    return "Confirm deletion";
  if (item.reason === "permission") return "Permission requested";
  if (item.reason === "question") return "Question for you";
  return item.reason;
}
