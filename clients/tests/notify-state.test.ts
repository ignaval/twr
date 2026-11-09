import { describe, expect, it } from "vitest";
import {
  AttentionWatch,
  BURST_MS,
  Burst,
  answered,
  itemKey,
  notices,
  stillWaiting,
} from "../desktop/notify-state";
import type { AttentionSummary, AttentionSummaryItem } from "../shared/types";

const item = (
  id: string,
  workspace = "default",
  extra: Partial<AttentionSummaryItem> = {},
): AttentionSummaryItem => ({
  workspace,
  thread: `t-${id}`,
  title: `thread-${id}`,
  summary: `Bash rm -rf ${id}`,
  id,
  revision: 1,
  reason: "permission",
  ...extra,
});
const summary = (
  items: AttentionSummaryItem[],
  streams: Record<string, string | undefined> = { default: "live:1" },
): AttentionSummary => ({
  count: items.length,
  items,
  workspaces: Object.entries(streams).map(([name, stream]) =>
    stream ? { name, stream } : { name },
  ),
});

describe("attention watch", () => {
  it("takes what exists at startup as known, and later requests as news", () => {
    const watch = new AttentionWatch();
    expect(watch.observe(summary([item("a")]), false)).toEqual([]);
    expect(
      watch.observe(summary([item("a"), item("b")]), false).map((i) => i.id),
    ).toEqual(["b"]);
    // The same request again is not news.
    expect(watch.observe(summary([item("a"), item("b")]), false)).toEqual([]);
  });

  it("notifies once per id and revision", () => {
    const watch = new AttentionWatch();
    watch.observe(summary([item("a")]), false);
    expect(
      watch.observe(summary([item("a", "default", { revision: 2 })]), false),
    ).toHaveLength(1);
    // Answered and raised again with a revision already seen: not news.
    watch.observe(summary([]), false);
    expect(
      watch.observe(summary([item("a", "default", { revision: 2 })]), false),
    ).toEqual([]);
  });

  it("takes what a reconnect, resync or switch lists first as known", () => {
    const watch = new AttentionWatch();
    watch.observe(summary([]), false);
    // Disconnected: the stream is gone.
    watch.observe(summary([], { default: undefined }), false);
    // The new connection replays what waited all along.
    expect(
      watch.observe(summary([item("a")], { default: "live:2" }), false),
    ).toEqual([]);
    // A resync starts another stream at once.
    expect(
      watch.observe(
        summary([item("a"), item("b")], { default: "live:3" }),
        false,
      ),
    ).toEqual([]);
    expect(
      watch.observe(
        summary([item("a"), item("b"), item("c")], { default: "live:3" }),
        false,
      ),
    ).toHaveLength(1);
  });

  it("follows each workspace's stream on its own", () => {
    const watch = new AttentionWatch();
    const streams = { default: "live:1", side: "read:local" };
    watch.observe(summary([], streams), false);
    expect(
      watch.observe(summary([item("s", "side")], streams), false),
    ).toHaveLength(1);
    // The side workspace's read failed, then came back: what it lists is not news.
    watch.observe(summary([], { ...streams, side: undefined }), false);
    expect(
      watch.observe(summary([item("s2", "side")], streams), false),
    ).toEqual([]);
    // A workspace seen for the first time lists only what was there.
    expect(
      watch.observe(
        summary([item("n", "new")], { ...streams, new: "read:local" }),
        false,
      ),
    ).toEqual([]);
  });

  it("never notifies what appears while the user is looking", () => {
    const watch = new AttentionWatch();
    watch.observe(summary([]), false);
    expect(watch.observe(summary([item("a")]), true)).toEqual([]);
    expect(watch.observe(summary([item("a")]), false)).toEqual([]);
  });

  it("waits for an incomplete request to be complete", () => {
    const watch = new AttentionWatch();
    watch.observe(summary([]), false);
    expect(
      watch.observe(
        summary([item("a", "default", { incomplete: true })]),
        false,
      ),
    ).toEqual([]);
    expect(watch.observe(summary([item("a")]), false)).toHaveLength(1);
  });
});

describe("notices", () => {
  it("name the thread, workspace and kind, never what is asked", () => {
    const [permission, question] = notices([
      item("a"),
      item("b", "client-x", { reason: "question", title: "web" }),
    ]);
    expect(permission).toMatchObject({
      title: "thread-a · default",
      body: "Asks for permission",
      target: { workspace: "default", thread: "t-a" },
      keys: [itemKey(item("a"))],
    });
    expect(question).toMatchObject({
      title: "web · client-x",
      body: "Has a question for you",
    });
    for (const notice of [permission, question])
      expect(notice.title + notice.body).not.toContain("rm -rf");
  });

  it("only for permissions and questions", () => {
    expect(notices([item("a", "default", { reason: "review" })])).toEqual([]);
  });

  it("keep a thread name on one line", () => {
    const [notice] = notices([
      item("a", "default", { title: "x\n\u001b[2Jy" }),
    ]);
    expect(notice.title).toBe("x [2Jy · default");
  });

  it("become one for a burst of more than three", () => {
    const three = notices([item("a"), item("b"), item("c")]);
    expect(three).toHaveLength(3);
    const burst = notices([
      item("a"),
      item("b"),
      item("c", "side"),
      item("d", "side", { reason: "question" }),
    ]);
    expect(burst).toEqual([
      {
        title: "4 things need you",
        body: "2 in default and 2 in side",
        keys: ["a", "b", "c", "d"].map((id, i) =>
          itemKey(item(id, i < 2 ? "default" : "side")),
        ),
        target: {},
      },
    ]);
    const one = notices(["a", "b", "c", "d"].map((id) => item(id, "side")));
    expect(one[0]).toMatchObject({
      body: "In side",
      target: { workspace: "side" },
    });
  });
});

describe("burst", () => {
  it("holds requests for a second from the first", () => {
    const burst = new Burst();
    expect(burst.dueIn(0)).toBeUndefined();
    burst.add([item("a")], 1000);
    burst.add([item("b")], 1600);
    expect(burst.dueIn(1600)).toBe(BURST_MS - 600);
    expect(burst.dueIn(5000)).toBe(0);
    expect(burst.take().map((i) => i.id)).toEqual(["a", "b"]);
    expect(burst.dueIn(5000)).toBeUndefined();
    burst.add([], 6000);
    expect(burst.dueIn(6000)).toBeUndefined();
  });
});

describe("answered", () => {
  it("once every request is gone from a current workspace", () => {
    const keys = [itemKey(item("a")), itemKey(item("s", "side"))];
    const streams = { default: "live:1", side: "read:local" };
    expect(answered(keys, summary([item("a")], streams))).toBe(false);
    expect(answered(keys, summary([item("s", "side")], streams))).toBe(false);
    expect(answered(keys, summary([], streams))).toBe(true);
    // A workspace that is not current tells nothing.
    expect(answered(keys, summary([], { ...streams, side: undefined }))).toBe(
      false,
    );
    // Raised again with a new revision: the old request was answered.
    expect(
      answered(
        [itemKey(item("a"))],
        summary([item("a", "default", { revision: 2 })]),
      ),
    ).toBe(true);
  });

  it("not from a summary that lists only part of what waits", () => {
    expect(answered([itemKey(item("a"))], { ...summary([]), count: 300 })).toBe(
      false,
    );
  });

  it("drops held requests answered meanwhile", () => {
    expect(
      stillWaiting([item("a"), item("b")], summary([item("b")])).map(
        (i) => i.id,
      ),
    ).toEqual(["b"]);
  });
});
