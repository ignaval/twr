import { describe, expect, it, vi } from "vitest";
import { localInboxSources } from "../desktop/inbox";
describe("local workspace Inbox reads", () => {
  it("uses independent read connections, closes every one and reports unavailable workspaces", async () => {
    const closes: string[] = [];
    const calls: string[] = [];
    const sources = await localInboxSources(
      [{ name: "default" }, { name: "work" }, { name: "stopped" }],
      (workspace) => ({
        close: () => {
          closes.push(workspace);
        },
        call: async (method) => {
          calls.push(method);
          if (workspace === "stopped") throw new Error("Hub is stopped");
          if (method === "hub.info") return { workspace };
          return method === "attention.list"
            ? [{ attn_id: workspace, revision: 1 }]
            : [{ id: workspace }];
        },
      }),
    );
    expect(sources.map((source) => source.workspace)).toEqual([
      "default",
      "work",
      "stopped",
    ]);
    expect(sources[0].attention).toEqual([{ attn_id: "default", revision: 1 }]);
    expect(sources[2]).toMatchObject({
      error: "Hub is stopped",
      attention: [],
      threads: [],
    });
    expect(closes.sort()).toEqual(["default", "stopped", "work"]);
    expect(calls).not.toContain("desktop.ensure_hub");
  });
  it("rejects a socket serving another workspace", async () => {
    const close = vi.fn();
    const call = vi.fn(async () => ({ workspace: "other" }));
    const [source] = await localInboxSources(["default"], () => ({
      close,
      call,
    }));
    expect(source.error).toContain("another workspace");
    expect(call).toHaveBeenCalledOnce();
    expect(close).toHaveBeenCalledOnce();
  });
  it("refuses unbounded and malformed workspace lists before opening a connection", async () => {
    const reader = vi.fn();
    await expect(
      localInboxSources(new Array(257).fill("default"), reader),
    ).rejects.toThrow();
    await expect(localInboxSources(["../outside"], reader)).rejects.toThrow();
    expect(reader).not.toHaveBeenCalled();
  });
});
