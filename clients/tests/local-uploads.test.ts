import { afterEach, describe, expect, it, vi } from "vitest";
import { File } from "node:buffer";
import { webcrypto } from "node:crypto";
import { localUploadRequest } from "../desktop/upload-request";
import { withLocalUploads } from "../shared/local-uploads";
import type { DesktopAPI } from "../shared/types";

afterEach(() => vi.unstubAllGlobals());
describe("native local attachment lane", () => {
  it("uses the selected thread RPC and bounded chunks for local desktop", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const data = new Uint8Array(140000).fill(42);
    let received = 0;
    const bridge = {
      call: vi.fn(async (method: string, params: any) => {
        expect(method).toBe("thread.upload");
        expect(params.thread).toBe("thread-id");
        expect(params).not.toHaveProperty("root");
        if (params.action === "chunk") {
          expect(atob(params.data).length).toBeLessThanOrEqual(65536);
          expect(params.offset).toBe(received);
          received += atob(params.data).length;
        }
        return {
          received,
          chunk_size: 262144,
          path:
            params.action === "commit"
              ? "/local/worktree/.twr-uploads/file"
              : null,
        };
      }),
    };
    const request = localUploadRequest(() => ({
      workspace: "default",
      ready: true,
      bridge,
    }));
    const api = withLocalUploads(
      { bootstrap: async () => ({ workspace: "default" }) } as DesktopAPI,
      request,
    );
    const out = await api.uploadFile!(
      new File([data], "file.bin") as unknown as globalThis.File,
      "thread-id",
    );
    expect(out.path).toBe("/local/worktree/.twr-uploads/file");
    expect(received).toBe(data.length);
    expect(bridge.call).toHaveBeenCalledTimes(5);
  });
  it("refuses changed workspaces, arbitrary paths, methods, and oversized chunks", async () => {
    const bridge = { call: vi.fn(async () => ({})) };
    const request = localUploadRequest(() => ({
      workspace: "default",
      ready: true,
      bridge,
    }));
    const params = { id: "file", thread: "thread", sha256: "a".repeat(64) };
    await expect(request("other", "upload-status", params)).rejects.toThrow(
      "Workspace changed",
    );
    await expect(request("default", "write-file", params)).rejects.toThrow();
    await expect(
      request("default", "upload-status", { ...params, root: "/escape" }),
    ).rejects.toThrow();
    await expect(
      request("default", "upload-chunk", {
        ...params,
        offset: 0,
        data: "A".repeat(87388),
      }),
    ).rejects.toThrow();
    expect(bridge.call).not.toHaveBeenCalled();
  });
  it("captures the admitted bridge while a later workspace connection changes", async () => {
    let finish!: (value: unknown) => void;
    const first = {
      call: vi.fn(
        () =>
          new Promise((resolve) => {
            finish = resolve;
          }),
      ),
    };
    const second = { call: vi.fn(async () => ({})) };
    let current = { workspace: "first", ready: true, bridge: first };
    const request = localUploadRequest(() => current);
    const pending = request("first", "upload-status", {
      id: "file",
      thread: "thread",
      sha256: "a".repeat(64),
    });
    current = { workspace: "second", ready: true, bridge: second };
    finish({ received: 12 });
    expect(await pending).toMatchObject({ received: 12 });
    expect(second.call).not.toHaveBeenCalled();
  });
});
