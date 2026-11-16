import type {
  AttentionSummary,
  AttentionSummaryItem,
  LoopEndSummary,
} from "../shared/types";

/* What the desktop notifies, decided without Electron so it is tested on its own
 * (desktop/notifier.ts posts what these return). docs/notifications.md › The desktop app. */

/**
 * The kinds of request that post an OS notification. The hub's confirm and delete cards are
 * questions too ({@link AttentionSummaryItem.card}), worded as what they are.
 */
export const NOTIFIED_REASONS: ReadonlySet<string> = new Set([
  "permission",
  "question",
]);
/** New requests are held this long, so that a burst goes out together. */
export const BURST_MS = 1000;
/** A burst of more than this many becomes one notification. */
export const BURST_MAX = 3;
/** How many requests the watch remembers having seen. */
const SEEN_LIMIT = 5000;

/** One request: an attention item's id and revision, in its workspace. */
export const itemKey = (
  item: Pick<AttentionSummaryItem, "workspace" | "id" | "revision">,
) => JSON.stringify([item.workspace, item.id, item.revision]);

/**
 * Tells which requests of each summary are news. A request is news once: the first
 * summary to list it, complete, from a workspace whose stream is the one of the summary
 * before. What a workspace lists when it starts a stream (at startup, after a reconnect or
 * a resync, when its read comes back) was there before and is taken as known, as is what
 * appears while the user is looking at the window.
 */
export class AttentionWatch {
  private streams = new Map<string, string>();
  private seen = new Set<string>();
  observe(
    summary: AttentionSummary,
    attended: boolean,
  ): AttentionSummaryItem[] {
    const continuing = new Set<string>();
    const streams = new Map<string, string>();
    for (const workspace of summary.workspaces)
      if (workspace.stream) {
        streams.set(workspace.name, workspace.stream);
        if (this.streams.get(workspace.name) === workspace.stream)
          continuing.add(workspace.name);
      }
    this.streams = streams;
    const news: AttentionSummaryItem[] = [];
    for (const item of summary.items) {
      const key = itemKey(item);
      if (this.seen.has(key)) continue;
      const fresh = continuing.has(item.workspace);
      // A request still being raised is news once it is complete.
      if (fresh && item.incomplete) continue;
      this.remember(key);
      if (fresh && !attended) news.push(item);
    }
    return news;
  }
  private remember(key: string) {
    this.seen.add(key);
    if (this.seen.size > SEEN_LIMIT)
      this.seen.delete(this.seen.values().next().value!);
  }
}

/** One loop's end: its thread and id, in its workspace. */
export const loopKey = (
  end: Pick<LoopEndSummary, "workspace" | "thread" | "id">,
) => JSON.stringify(["loop", end.workspace, end.thread, end.id]);

/**
 * Tells which loop ends of each summary are news, as {@link AttentionWatch} does requests: a
 * loop's end is news once, the first summary to list it from a workspace whose stream is the
 * one of the summary before, while the user is not looking at the window. What a workspace
 * lists when its stream starts (the threads' last loops, ended before) is known.
 */
export class LoopWatch {
  private streams = new Map<string, string>();
  private seen = new Set<string>();
  observe(summary: AttentionSummary, attended: boolean): LoopEndSummary[] {
    const continuing = new Set<string>();
    const streams = new Map<string, string>();
    for (const workspace of summary.workspaces)
      if (workspace.stream) {
        streams.set(workspace.name, workspace.stream);
        if (this.streams.get(workspace.name) === workspace.stream)
          continuing.add(workspace.name);
      }
    this.streams = streams;
    const news: LoopEndSummary[] = [];
    for (const end of summary.loops || []) {
      const key = loopKey(end);
      if (this.seen.has(key)) continue;
      this.seen.add(key);
      if (this.seen.size > SEEN_LIMIT)
        this.seen.delete(this.seen.values().next().value!);
      if (continuing.has(end.workspace) && !attended) news.push(end);
    }
    return news;
  }
}

/**
 * The notifications for loops that ended: one each, with how it ended (`Review loop clean
 * after 6 rounds`; an implementation loop's commits, then `not committed: …` for any
 * repository that was not), or one for all of
 * them past {@link BURST_MAX}. Their keys are the loops', which no answer removes: they go
 * when clicked or when the window comes to the front.
 */
export function loopNotices(ends: LoopEndSummary[]): Notice[] {
  if (ends.length > BURST_MAX) {
    const workspaces = [...new Set(ends.map((end) => end.workspace))];
    return [
      {
        title: `${ends.length} loops ended`,
        body: list(ends.slice(0, 6).map((end) => plain(end.title, 40))),
        keys: ends.map(loopKey),
        target: workspaces.length === 1 ? { workspace: workspaces[0] } : {},
      },
    ];
  }
  return ends.map((end) => {
    const words = plain(end.words, 160);
    return {
      title: `${plain(end.title, 80)} · ${plain(end.workspace, 40)}`,
      body: words.charAt(0).toUpperCase() + words.slice(1),
      keys: [loopKey(end)],
      target: { workspace: end.workspace, thread: end.thread },
    };
  });
}

/** Whether a notification's keys are loop ends ({@link loopKey}): no answer removes them. */
export const isLoopNotice = (keys: string[]) =>
  keys.length > 0 && keys.every((key) => key.startsWith('["loop",'));

/** Requests held for {@link BURST_MS} after the first, so that a burst goes out as one. */
export class Burst {
  private items: AttentionSummaryItem[] = [];
  private since?: number;
  add(items: AttentionSummaryItem[], now: number) {
    if (!items.length) return;
    if (this.since === undefined) this.since = now;
    this.items.push(...items);
  }
  /** Milliseconds until the held requests are due; undefined when none are held. */
  dueIn(now: number) {
    return this.since === undefined
      ? undefined
      : Math.max(0, BURST_MS - (now - this.since));
  }
  take(): AttentionSummaryItem[] {
    const items = this.items;
    this.items = [];
    this.since = undefined;
    return items;
  }
  clear() {
    this.take();
  }
}

/** Where a click on a notification takes the window. */
export interface NoticeTarget {
  workspace?: string;
  thread?: string;
}
export interface Notice {
  title: string;
  body: string;
  /** The requests it is about ({@link itemKey}); it is removed once all are answered. */
  keys: string[];
  target: NoticeTarget;
}

/** No control characters: a thread name cannot start a line of its own. */
const plain = (text: string, max = 120) => {
  const single = text
    .replace(/[\u0000-\u001f\u007f-\u009f]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return single.length > max ? single.slice(0, max - 1) + "…" : single;
};

/** What the request asks, in a few words that say nothing of its content. */
export const kind = (item: Pick<AttentionSummaryItem, "reason" | "card">) =>
  item.card === "delete"
    ? "Asks you to confirm a deletion"
    : item.card === "loop_setup"
      ? "Asks you to choose a loop's settings"
      : item.card === "loop_stuck"
        ? "Has a loop that is stuck"
        : item.card === "confirm"
          ? "Asks you to confirm what the orchestrator proposed"
          : item.reason === "permission"
            ? "Asks for permission"
            : item.reason === "question"
              ? "Has a question for you"
              : "Needs you";

const list = (words: string[]) =>
  words.length <= 1
    ? words.join("")
    : `${words.slice(0, -1).join(", ")} and ${words.at(-1)}`;

/**
 * The notifications for requests that came due together: one each, or one for all of them
 * when there are more than {@link BURST_MAX}. They name the thread (`twr` for the hub's own
 * cards), the workspace and the kind of request (a deletion says so), never what is asked
 * (a command line, a question, the threads a card would delete): that stays in the app,
 * as in the hub's notifications without `detail` and in mobile push.
 */
export function notices(items: AttentionSummaryItem[]): Notice[] {
  const wanted = items.filter(
    (item) => NOTIFIED_REASONS.has(item.reason) || item.card !== undefined,
  );
  if (wanted.length > BURST_MAX) {
    const workspaces = [...new Set(wanted.map((item) => item.workspace))];
    const counts = workspaces.map(
      (name) =>
        `${wanted.filter((item) => item.workspace === name).length} in ${plain(name, 40)}`,
    );
    return [
      {
        title: `${wanted.length} things need you`,
        body:
          workspaces.length > 1
            ? list(counts)
            : `In ${plain(workspaces[0], 40)}`,
        keys: wanted.map(itemKey),
        target: workspaces.length === 1 ? { workspace: workspaces[0] } : {},
      },
    ];
  }
  return wanted.map((item) => ({
    title: `${plain(item.title, 80)} · ${plain(item.workspace, 40)}`,
    body: kind(item),
    keys: [itemKey(item)],
    target: { workspace: item.workspace, thread: item.thread },
  }));
}

/**
 * Whether a notification's requests have all been answered as far as this summary tells:
 * each is gone from a workspace whose items are current. A workspace that is not current
 * (connecting, a failed read) says nothing either way.
 */
export function answered(keys: string[], summary: AttentionSummary): boolean {
  // A summary that lists only some of what waits cannot tell what was answered.
  if (summary.count > summary.items.length) return false;
  const current = new Set(
    summary.workspaces.filter((w) => w.stream).map((w) => w.name),
  );
  const waiting = new Set(summary.items.map(itemKey));
  return keys.every((key) => {
    const [workspace] = JSON.parse(key) as [string];
    return current.has(workspace) && !waiting.has(key);
  });
}

/** The requests of `items` that a summary still lists, or cannot tell about. */
export function stillWaiting(
  items: AttentionSummaryItem[],
  summary: AttentionSummary,
) {
  return items.filter((item) => !answered([itemKey(item)], summary));
}
