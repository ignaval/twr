import { describe, expect, it } from "vitest";
import type { InboxSource, Thread } from "../shared/types";
import { peekThreads, workspaceEntries } from "../shared/workspaceTree";

const thread = (id: string, status: string, extra: Partial<Thread> = {}) =>
  ({
    id,
    name: id,
    runtime: "fake",
    box_name: "local",
    mode: "auto",
    status,
    accent: "#888",
    is_orchestrator: false,
    ...extra,
  }) as Thread;
const source = (
  workspace: string,
  threads: Thread[],
  extra: Partial<InboxSource> = {},
): InboxSource => ({
  hubId: "local",
  hubName: "This computer",
  workspace,
  attention: [],
  threads,
  ...extra,
});

describe("workspace tree", () => {
  it("lists another workspace's threads as the TUI does", () => {
    const threads = [
      thread("orch", "idle", { is_orchestrator: true }),
      thread("idle-1", "idle"),
      thread("busy", "running"),
      thread("asleep", "parked"),
      thread("broken", "error"),
      thread("asks", "idle"),
      thread("bg", "background"),
    ];
    const peek = peekThreads({
      threads,
      attention: [
        {
          attn_id: "a",
          revision: 1,
          thread: "asks",
          reason: "permission",
          summary: "",
          options: [],
        },
      ],
    });
    expect(peek.threads.map((t) => t.id)).toEqual([
      "asks",
      "broken",
      "busy",
      "bg",
      "idle-1",
    ]);
    expect(peek.more).toBe(0);
  });

  it("cuts long lists with a row counting the rest", () => {
    const many = Array.from({ length: 9 }, (_, i) => thread(`t${i}`, "idle"));
    const peek = peekThreads({ threads: many, attention: [] }, 6);
    expect(peek.threads).toHaveLength(5);
    expect(peek.more).toBe(4);
    expect(
      peekThreads({ threads: many.slice(0, 6), attention: [] }, 6),
    ).toEqual({ threads: many.slice(0, 6), more: 0 });
    expect(peekThreads(undefined)).toEqual({ threads: [], more: 0 });
  });

  it("orders workspaces default first and fills them from live and scanned data", () => {
    const entries = workspaceEntries(
      [
        { name: "zeta", hub: "stopped" },
        { name: "default", hub: "running" },
        { name: "alpha", hub: "running", waiting: 2, running: 1 },
        "beta",
      ],
      "beta",
      "local",
      [
        source("zeta", [thread("old", "idle")], {
          running: false,
          cached: true,
        }),
        source("default", [thread("x", "running"), thread("y", "waiting")], {
          attention: [
            {
              attn_id: "q",
              revision: 1,
              thread: "y",
              reason: "question",
              summary: "",
              options: [],
            },
          ],
        }),
        // Another hub's workspace of the same name is not this one.
        source("alpha", [thread("elsewhere", "running")], { hubId: "remote" }),
      ],
      { threads: [thread("mine", "running")], attention: [] },
    );
    expect(entries.map((e) => e.name)).toEqual([
      "default",
      "alpha",
      "beta",
      "zeta",
    ]);
    const [def, alpha, beta, zeta] = entries;
    expect(def).toMatchObject({
      current: false,
      running: true,
      waiting: 1,
      working: 1,
      stale: false,
    });
    expect(def.threads.map((t) => t.id)).toEqual(["y", "x"]);
    // Not scanned yet: the counts `twr ws list` gave, and no rows.
    expect(alpha).toMatchObject({
      running: undefined,
      waiting: 2,
      working: 1,
      threads: [],
    });
    // The current workspace lists its live cards itself.
    expect(beta).toMatchObject({ current: true, working: 1, threads: [] });
    expect(zeta).toMatchObject({ running: false, stale: true });
    expect(zeta.threads.map((t) => t.id)).toEqual(["old"]);
  });

  it("adds workspaces a live scan found, not ones only a saved summary names", () => {
    const names = workspaceEntries(
      ["default"],
      "default",
      "local",
      [
        source("fresh", []),
        source("removed", [], { cached: true }),
        source("theirs", [], { hubId: "remote" }),
      ],
      { threads: [], attention: [] },
    ).map((e) => e.name);
    expect(names).toEqual(["default", "fresh"]);
  });

  it("shows a connecting workspace as last scanned, so its rows stay put", () => {
    const [entry] = workspaceEntries(
      ["beta"],
      "beta",
      "local",
      [source("beta", [thread("api", "running")])],
      undefined,
    );
    expect(entry).toMatchObject({ current: true, working: 1 });
    expect(entry.threads.map((t) => t.id)).toEqual(["api"]);
  });

  it("keeps the current workspace when the listing has not loaded", () => {
    expect(
      workspaceEntries([], "default", "local", [], {
        threads: [],
        attention: [],
      }).map((e) => e.name),
    ).toEqual(["default"]);
  });
});
