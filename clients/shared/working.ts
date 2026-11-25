import type { Attention, HubEvent, Json, Thread } from "./types";

/** The conversation's working cursor, as the TUI's (docs/tui.md, "What is working"): a `_`
 * that breathes up to a block and back while the thread the conversation shows runs a
 * turn, where its next text will appear. */

/** Whether `thread` runs a turn or background tasks (twr-tui `icon::is_working`): the
 * hub's fold says `waiting` while a permission or a question blocks on the user. */
export function threadWorks(thread: Pick<Thread, "status"> | undefined) {
  return thread?.status === "running" || thread?.status === "background";
}

/** Whether `thread` waits on the user: its status says so, or a permission or a question
 * of its is in the view already while the status catches up. */
export function waitsOnUser(
  thread: Pick<Thread, "id" | "status">,
  attention: readonly Pick<Attention, "thread" | "reason">[],
) {
  return (
    thread.status === "waiting" ||
    attention.some(
      (item) =>
        item.thread === thread.id &&
        (item.reason === "permission" || item.reason === "question"),
    )
  );
}

/** A message sent to `thread` whose turn the view doesn't show running yet: from the send
 * until the hub says the thread works (or waits on the user), or a turn ends after it. */
export interface AwaitedTurn {
  thread: string;
  /** The last `turn.ended` of the conversation when the send began. */
  endedBefore?: string;
  /** When the send began (ms): a turn that never comes stops being awaited. */
  at: number;
}

/** How long a sent message is awaited without the hub saying its turn runs. */
export const AWAIT_LIMIT_MS = 30_000;

/** The id of the last `turn.ended` in `events`. */
export function lastTurnEnd(events: readonly HubEvent[]): string | undefined {
  const end = events.findLast((event) => event.kind === "turn.ended");
  return end ? end.id || String(end.seq) : undefined;
}

/** Whether `awaited` still bridges the gap before `thread`'s turn shows running. */
export function stillAwaited(
  awaited: AwaitedTurn | undefined,
  thread: Pick<Thread, "id" | "status"> | undefined,
  events: readonly HubEvent[],
  now: number,
) {
  return (
    !!awaited &&
    !!thread &&
    awaited.thread === thread.id &&
    // The hub's status is authoritative from the moment it says the thread works or
    // waits: it covers the rest of the turn.
    thread.status !== "running" &&
    thread.status !== "background" &&
    thread.status !== "waiting" &&
    lastTurnEnd(events) === awaited.endedBefore &&
    now - awaited.at < AWAIT_LIMIT_MS
  );
}

/** Whether the conversation with `thread` shows the working cursor: a message to it is in
 * flight or awaited, or it works; never while it waits on the user. */
export function conversationWorks({
  thread,
  attention,
  sending,
  awaited,
}: {
  thread: Pick<Thread, "id" | "status"> | undefined;
  attention: readonly Pick<Attention, "thread" | "reason">[];
  /** A message to this conversation is in flight. */
  sending: boolean;
  /** {@link stillAwaited}. */
  awaited: boolean;
}) {
  if (!thread || waitsOnUser(thread, attention)) return false;
  return sending || awaited || threadWorks(thread);
}

/** Where the cursor goes: at the end of a reply that streams in, else on a line of its own
 * below the last message. */
export function cursorPlace(
  last: { kind: string; pending?: boolean } | undefined,
): "inline" | "line" {
  return last?.kind === "assistant" && last.pending ? "inline" : "line";
}

/** `[tui] animate` from `config.show` (the file as it is now, then what the hub loaded):
 * `false` keeps the cursor a still `_`. An older hub says nothing, and it moves. */
export function animateSetting(config: Json): boolean {
  const now = config?.preferences?.["tui.animate"];
  if (typeof now === "boolean") return now;
  return config?.tui?.animate !== false;
}

/** The id of the latest `config.changed` of `tui.animate` in `events`: a new one means
 * the setting changed, and `config.show` says to what (an old one in the history may no
 * longer be what the file says). */
export function animateChange(events: readonly HubEvent[]): string | undefined {
  const change = events.findLast(
    (event) =>
      event.kind === "config.changed" && event.data?.key === "tui.animate",
  );
  return change ? change.id || String(change.seq) : undefined;
}

/** The element of `root` the inline cursor follows: the one holding its last visible
 * text, so the cursor sits where the next text will appear (`root` itself when it holds
 * none). */
export function cursorHost(root: Element): Element {
  const last = (node: Node): Element | undefined => {
    for (let child = node.lastChild; child; child = child.previousSibling) {
      if (child.nodeType === 3 /* text */) {
        if (child.textContent?.trim()) return node as Element;
      } else if (child.nodeType === 1 /* element */) {
        const found = last(child);
        if (found) return found;
      }
    }
    return undefined;
  };
  return last(root) ?? root;
}
