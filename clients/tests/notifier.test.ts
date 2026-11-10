import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
vi.mock("electron", () => ({ Notification: { isSupported: () => true } }));
import { Notifier, type Post } from "../desktop/notifier";
import { BURST_MS, type NoticeTarget } from "../desktop/notify-state";
import type { AttentionSummary, AttentionSummaryItem } from "../shared/types";

const item = (id: string, workspace = "default"): AttentionSummaryItem => ({
  workspace,
  thread: `t-${id}`,
  title: `thread-${id}`,
  summary: "Bash cargo test",
  id,
  revision: 1,
  reason: "permission",
});
const summary = (items: AttentionSummaryItem[]): AttentionSummary => ({
  count: items.length,
  items,
  workspaces: [
    { name: "default", stream: "live:1" },
    { name: "side", stream: "read:local" },
  ],
});

function setup({ supported = true, enabled = true } = {}) {
  const state = { attended: false, enabled };
  const posted: {
    title: string;
    body: string;
    closed: boolean;
    click(): void;
  }[] = [];
  const opened: NoticeTarget[] = [];
  const log = { info: vi.fn(), warn: vi.fn() };
  const post: Post = (notice, handlers) => {
    const record = { ...notice, closed: false, click: handlers.click };
    posted.push(record);
    return { close: () => (record.closed = true) };
  };
  const notifier = new Notifier(
    {
      enabled: () => state.enabled,
      attended: () => state.attended,
      open: (target) => opened.push(target),
      log: log as any,
    },
    { post, supported, now: () => Date.now() },
  );
  notifier.update(summary([]));
  return { notifier, state, posted, opened, log };
}

describe("notifier", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("posts a new request after the burst wait, and a click only opens it", () => {
    const { notifier, posted, opened } = setup();
    notifier.update(summary([item("a", "side")]));
    expect(posted).toHaveLength(0);
    vi.advanceTimersByTime(BURST_MS);
    expect(posted.map((p) => [p.title, p.body])).toEqual([
      ["thread-a · side", "Asks for permission"],
    ]);
    posted[0].click();
    expect(opened).toEqual([{ workspace: "side", thread: "t-a" }]);
    expect(posted[0].closed).toBe(true);
  });

  it("removes a notification once its request is answered", () => {
    const { notifier, posted } = setup();
    notifier.update(summary([item("a")]));
    vi.advanceTimersByTime(BURST_MS);
    notifier.update(summary([item("a"), item("b")]));
    expect(posted[0].closed).toBe(false);
    notifier.update(summary([item("b")]));
    expect(posted[0].closed).toBe(true);
  });

  it("posts nothing for a request answered while held", () => {
    const { notifier, posted } = setup();
    notifier.update(summary([item("a")]));
    notifier.update(summary([]));
    vi.advanceTimersByTime(BURST_MS);
    expect(posted).toHaveLength(0);
  });

  it("posts nothing while the window is in front, and clears when it comes back", () => {
    const { notifier, state, posted } = setup();
    state.attended = true;
    notifier.update(summary([item("a")]));
    vi.advanceTimersByTime(BURST_MS);
    expect(posted).toHaveLength(0);
    state.attended = false;
    notifier.update(summary([item("a"), item("b")]));
    vi.advanceTimersByTime(BURST_MS);
    expect(posted).toHaveLength(1);
    notifier.attend();
    expect(posted[0].closed).toBe(true);
  });

  it("coalesces a burst into one", () => {
    const { notifier, posted } = setup();
    notifier.update(summary([item("a"), item("b")]));
    vi.advanceTimersByTime(BURST_MS / 2);
    notifier.update(summary(["a", "b", "c", "d"].map((id) => item(id))));
    vi.advanceTimersByTime(BURST_MS);
    expect(posted.map((p) => p.title)).toEqual(["4 things need you"]);
  });

  it("posts nothing when turned off, nor later for what came meanwhile", () => {
    const { notifier, state, posted } = setup({ enabled: false });
    notifier.update(summary([item("a")]));
    vi.advanceTimersByTime(BURST_MS);
    state.enabled = true;
    notifier.update(summary([item("a")]));
    vi.advanceTimersByTime(BURST_MS);
    expect(posted).toHaveLength(0);
  });

  it("logs once where notifications are not supported", () => {
    const { notifier, posted, log } = setup({ supported: false });
    notifier.update(summary([item("a")]));
    notifier.update(summary([item("a"), item("b")]));
    vi.advanceTimersByTime(BURST_MS);
    expect(posted).toHaveLength(0);
    expect(log.info).toHaveBeenCalledTimes(1);
  });
});
