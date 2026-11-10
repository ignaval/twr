import { afterEach, describe, expect, it, vi } from "vitest";
import {
  coalesceInboxSources,
  localInboxSources,
  WorkspaceListing,
} from "../desktop/inbox";
import type { InboxSource } from "../shared/types";
afterEach(() => vi.useRealTimers());
const summary = (workspace: string): InboxSource => ({
  hubId: "local",
  hubName: "This computer",
  workspace,
  attention: [],
  threads: [],
});
describe("workspace inbox reads", () => {
  it("bounds a large scan by one deadline and closes every started reader", async () => {
    vi.useFakeTimers();
    const closed: string[] = [];
    const reader = vi.fn((workspace: string) => ({
      call: () => new Promise(() => {}),
      close: () => {
        closed.push(workspace);
      },
    }));
    const workspaces = Array.from(
      { length: 64 },
      (_, index) => `workspace-${index}`,
    );
    const pending = localInboxSources(workspaces, reader, 4000);
    let done = false;
    void pending.then(() => {
      done = true;
    });
    await vi.advanceTimersByTimeAsync(3999);
    expect(done).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    const sources = await pending;
    expect(done).toBe(true);
    expect(reader).toHaveBeenCalledTimes(8);
    expect(closed).toHaveLength(8);
    expect(sources).toHaveLength(64);
    expect(sources.slice(0, 8).every((source) => !!source.error)).toBe(true);
    expect(
      sources
        .slice(8)
        .every(
          (source) => !source.cached && source.error?.includes("deadline"),
        ),
    ).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
  });
  it("returns completed summaries when the remaining readers exceed the deadline", async () => {
    vi.useFakeTimers();
    const closed = vi.fn();
    const sources = localInboxSources(
      ["ready", "blocked", "later"],
      (workspace) => ({
        call: async (method) =>
          workspace !== "ready"
            ? new Promise(() => {})
            : method === "hub.info"
              ? { workspace }
              : method === "attention.list"
                ? [{ attn_id: "review", revision: 12 }]
                : [{ id: "worker" }],
        close: closed,
      }),
      100,
    );
    await vi.advanceTimersByTimeAsync(100);
    const result = await sources;
    expect(result[0].attention[0]).toMatchObject({
      attn_id: "review",
      revision: 12,
    });
    expect(result[0].threads[0]).toMatchObject({ id: "worker" });
    expect(result.slice(1).every((source) => !!source.error)).toBe(true);
    expect(closed).toHaveBeenCalledTimes(3);
  });
  it("does not start a reader for a known stopped hub and treats refused sockets as stopped", async () => {
    const closed = vi.fn();
    const reader = vi.fn((workspace: string) => ({
      call: async () => {
        throw new Error(
          workspace === "refused"
            ? "Connection refused (os error 61)"
            : "unexpected read",
        );
      },
      close: closed,
    }));
    const result = await localInboxSources(
      [{ name: "stopped", hub: "stopped" }, "refused"],
      reader,
    );
    expect(reader).toHaveBeenCalledTimes(1);
    expect(reader).toHaveBeenCalledWith("refused");
    expect(result).toEqual([
      { ...summary("stopped"), running: false },
      { ...summary("refused"), running: false },
    ]);
    expect(closed).toHaveBeenCalledTimes(1);
  });
  it("keeps source errors distinct from stopped hubs once identity was established", async () => {
    const closed = vi.fn();
    const result = await localInboxSources(
      ["broken", "wrong", "start-failed"],
      (workspace) => {
        if (workspace === "start-failed")
          throw new Error("Cannot start the inbox reader");
        return {
          call: async (method) => {
            if (method === "hub.info")
              return {
                workspace: workspace === "wrong" ? "foreign" : workspace,
              };
            throw new Error("No such file: saved blob");
          },
          close: closed,
        };
      },
    );
    expect(result[0]).toMatchObject({ error: "No such file: saved blob" });
    expect(result[0].running).toBeUndefined();
    expect(result[1]).toMatchObject({
      error: "Socket serves another workspace",
    });
    expect(result[2]).toMatchObject({ error: "Cannot start the inbox reader" });
    expect(closed).toHaveBeenCalledTimes(2);
  });
});
describe("workspaces the listing read", () => {
  it("takes each running workspace from the listing, with no connection of its own", async () => {
    const reader = vi.fn(() => {
      throw new Error("unexpected reader");
    });
    const attention = [
      { attn_id: "question", reason: "question", revision: 2 },
    ];
    const threads = [{ id: "worker", name: "api", status: "waiting" }];
    const result = await localInboxSources(
      [
        { name: "default", hub: "running", detail: { attention, threads } },
        {
          name: "slow",
          hub: "busy",
          detail_error: "Workspace did not answer within 3 seconds",
        },
        { name: "odd", hub: "running", detail: { attention: {}, threads } },
        { name: "off", hub: "stopped" },
      ],
      reader,
    );
    expect(reader).not.toHaveBeenCalled();
    expect(result).toEqual([
      { ...summary("default"), attention, threads },
      {
        ...summary("slow"),
        error: "Workspace did not answer within 3 seconds",
      },
      { ...summary("odd"), error: "Invalid workspace summary" },
      { ...summary("off"), running: false },
    ]);
  });
  it("reads on its own only what the listing did not", async () => {
    const closed = vi.fn();
    const reader = vi.fn((workspace: string) => ({
      call: async (method: string) =>
        method === "hub.info"
          ? { workspace }
          : method === "attention.list"
            ? [{ attn_id: "old-cli" }]
            : [{ id: "worker" }],
      close: closed,
    }));
    const result = await localInboxSources(
      [
        {
          name: "default",
          hub: "running",
          detail: { attention: [], threads: [] },
        },
        { name: "older", hub: "running" },
      ],
      reader,
    );
    expect(reader.mock.calls).toEqual([["older"]]);
    expect(closed).toHaveBeenCalledTimes(1);
    expect(result[1]).toEqual({
      ...summary("older"),
      attention: [{ attn_id: "old-cli" }],
      threads: [{ id: "worker" }],
    });
  });
});
describe("inbox refresh coalescing", () => {
  it("shares an in-flight scan, then allows a fresh scan after completion", async () => {
    let resolve!: (value: InboxSource[]) => void;
    const read = vi.fn(
      () =>
        new Promise<InboxSource[]>((done) => {
          resolve = done;
        }),
    );
    const refresh = coalesceInboxSources(read);
    const first = refresh();
    const second = refresh();
    expect(first).toBe(second);
    await Promise.resolve();
    expect(read).toHaveBeenCalledTimes(1);
    resolve([summary("default")]);
    await expect(second).resolves.toEqual([summary("default")]);
    const third = refresh();
    expect(third).not.toBe(first);
    await Promise.resolve();
    expect(read).toHaveBeenCalledTimes(2);
    resolve([]);
    await third;
  });
  it("releases the shared scan after rejection so the user can retry", async () => {
    const read = vi
      .fn()
      .mockRejectedValueOnce(new Error("listing failed"))
      .mockResolvedValueOnce([]);
    const refresh = coalesceInboxSources(read);
    const first = refresh();
    const second = refresh();
    expect(second).toBe(first);
    await expect(first).rejects.toThrow("listing failed");
    await expect(refresh()).resolves.toEqual([]);
    expect(read).toHaveBeenCalledTimes(2);
  });
});
describe("workspace listing", () => {
  it("gives the listing one overall budget and falls back to the last good list", async () => {
    const list = vi
      .fn<(timeout: number, detail: boolean) => Promise<unknown>>()
      .mockResolvedValueOnce({ workspaces: [{ name: "default" }, "other"] })
      .mockRejectedValueOnce(new Error("Command timed out after 5000 ms"))
      .mockResolvedValueOnce("garbage")
      .mockResolvedValueOnce(["default"]);
    const listing = new WorkspaceListing(list, 5000);
    expect(await listing.get()).toEqual([{ name: "default" }, "other"]);
    expect(list).toHaveBeenLastCalledWith(5000, true);
    // A slow hub or a bad answer keeps the previous list.
    expect(await listing.get()).toEqual([{ name: "default" }, "other"]);
    expect(await listing.get()).toEqual([{ name: "default" }, "other"]);
    expect(await listing.get()).toEqual(["default"]);
  });

  it("asks for what each hub listed, and an older CLI for the list alone", async () => {
    const refused = Object.assign(new Error("Command failed"), {
      code: 2,
      stderr:
        "error: unexpected argument '--detail' found\n\nUsage: twr ws list --json\n",
    });
    const list = vi
      .fn<(timeout: number, detail: boolean) => Promise<unknown>>()
      .mockRejectedValueOnce(refused)
      .mockResolvedValue([{ name: "default", hub: "running" }]);
    const listing = new WorkspaceListing(list, 5000);
    expect(await listing.get()).toEqual([{ name: "default", hub: "running" }]);
    expect(await listing.get()).toEqual([{ name: "default", hub: "running" }]);
    expect(list.mock.calls).toEqual([
      [5000, true],
      [5000, false],
      [5000, false],
    ]);
  });

  it("falls back to the last list without what its hubs listed then", async () => {
    const detail = { threads: [{ id: "t" }], attention: [{ attn_id: "a" }] };
    const list = vi
      .fn<(timeout: number, detail: boolean) => Promise<unknown>>()
      .mockResolvedValueOnce([
        { name: "default", hub: "running", detail },
        { name: "busy", hub: "busy", detail_error: "Workspace did not answer" },
        { name: "off", hub: "stopped" },
      ])
      .mockRejectedValueOnce(new Error("Command timed out after 5000 ms"));
    const listing = new WorkspaceListing(list, 5000);
    expect((await listing.get())[0]).toMatchObject({ detail });
    expect(await listing.get()).toEqual([
      { name: "default", hub: "running" },
      { name: "busy", hub: "busy" },
      { name: "off", hub: "stopped" },
    ]);
    // Only a refusal of the flag itself turns it off.
    expect(list.mock.calls.every(([, detail]) => detail)).toBe(true);
  });

  it("reports a failure when no listing has worked yet", async () => {
    const listing = new WorkspaceListing(async () => {
      throw new Error("Command timed out");
    });
    await expect(listing.get()).rejects.toThrow("Command timed out");
  });
});
