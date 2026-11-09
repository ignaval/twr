import { describe, expect, it } from "vitest";
import { attentionSummary } from "../shared/attentionSummary";
import type { Attention, InboxSource, Thread } from "../shared/types";

const thread = (id: string, name = id) =>
  ({ id, name, status: "waiting" }) as Thread;
const ask = (id: string, threadId: string, reason = "permission") =>
  ({
    attn_id: id,
    revision: 1,
    thread: threadId,
    reason,
    summary: `Bash ${id}`,
    options: [],
  }) as Attention;
const source = (
  workspace: string,
  attention: Attention[],
  extra: Partial<InboxSource> = {},
): InboxSource => ({
  hubId: "local",
  hubName: "This computer",
  workspace,
  threads: attention.map((a) => thread(a.thread, `${a.thread}-name`)),
  attention,
  ...extra,
});

describe("attention summary", () => {
  it("merges the live view with the other workspaces' reads, the shown one first", () => {
    const summary = attentionSummary({
      current: "side",
      hubId: "local",
      live: { threads: [thread("t1", "api")], attention: [ask("a1", "t1")] },
      liveStream: "live:1",
      sources: [
        source("zeta", [ask("z1", "tz", "question")]),
        source("default", [ask("d1", "td")]),
        // The shown workspace's read is superseded by its live view.
        source("side", [ask("old", "t1")]),
        // Another hub's workspaces are not this one's.
        source("default", [ask("r1", "tr")], { hubId: "remote" }),
      ],
    });
    expect(summary.count).toBe(3);
    expect(summary.items.map((i) => [i.workspace, i.id, i.title])).toEqual([
      ["side", "a1", "api"],
      ["default", "d1", "td-name"],
      ["zeta", "z1", "tz-name"],
    ]);
    expect(summary.items[2]).toMatchObject({
      reason: "question",
      revision: 1,
      thread: "tz",
      summary: "Bash z1",
    });
    expect(summary.workspaces).toEqual([
      { name: "side", stream: "live:1" },
      { name: "default", stream: "read:local" },
      { name: "zeta", stream: "read:local" },
    ]);
  });

  it("marks saved, failed and stopped reads as not current", () => {
    const summary = attentionSummary({
      current: "default",
      hubId: "local",
      live: { threads: [], attention: [] },
      sources: [
        source("a", [ask("a1", "t")], { cached: true }),
        source("b", [], { error: "timed out" }),
        source("c", [], { running: false }),
        source("d", [], { scanError: true }),
      ],
    });
    expect(summary.workspaces).toEqual([
      { name: "default" },
      { name: "a" },
      { name: "b" },
      { name: "c" },
      { name: "d" },
    ]);
    // Saved items still count for the badge.
    expect(summary.count).toBe(1);
  });

  it("uses the shown workspace's last read while it connects, as not current", () => {
    const summary = attentionSummary({
      current: "side",
      hubId: "local",
      sources: [source("side", [ask("s1", "t")])],
    });
    expect(summary.items.map((i) => i.id)).toEqual(["s1"]);
    expect(summary.workspaces).toEqual([{ name: "side" }]);
  });

  it("counts everything but lists at most the maximum", () => {
    const summary = attentionSummary({
      current: "default",
      hubId: "local",
      live: {
        threads: [],
        attention: Array.from({ length: 5 }, (_, i) => ask(`a${i}`, "t")),
      },
      sources: [],
      max: 3,
    });
    expect(summary.count).toBe(5);
    expect(summary.items).toHaveLength(3);
    // A thread the view doesn't name keeps its id.
    expect(summary.items[0].title).toBe("t");
  });

  it("keeps incomplete requests marked", () => {
    const pending = { ...ask("a1", "t"), incomplete: true };
    const summary = attentionSummary({
      current: "default",
      hubId: "local",
      live: { threads: [], attention: [pending] },
      sources: [],
    });
    expect(summary.items[0].incomplete).toBe(true);
  });
});
