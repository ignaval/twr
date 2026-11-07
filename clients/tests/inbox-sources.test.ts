import { afterEach, describe, expect, it, vi } from "vitest";
import { coalesceInboxSources, localInboxSources } from "../desktop/inbox";
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
