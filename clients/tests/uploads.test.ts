import { afterEach, describe, expect, it, vi } from "vitest";
import { File } from "node:buffer";
import { webcrypto } from "node:crypto";
import {
  createUploadFile,
  UploadController,
  type UploadResume,
} from "../shared/uploads";
import { fileDigest } from "../shared/remote-crypto";

const file = (data = "hello") =>
  new File([data], "hello.txt", {
    type: "text/plain",
  }) as unknown as globalThis.File;
afterEach(() => vi.unstubAllGlobals());
describe("shared attachment transfers", () => {
  it("keeps queued transfers in their original workspace across switches", async () => {
    vi.stubGlobal("crypto", webcrypto);
    let workspace = "first";
    let finish!: () => void;
    const calls: string[] = [];
    const controller = new UploadController(
      {
        uploadFile: async (_file, _thread, options) => {
          calls.push(options!.workspace!);
          if (calls.length <= 2)
            await new Promise<void>((resolve) => {
              const previous = finish;
              finish = () => {
                previous?.();
                resolve();
              };
            });
          return { path: "/worker/file-" + calls.length };
        },
      },
      () => {},
    );
    controller.setWorkspace(workspace);
    controller.add([file(), file(), file()], "worker", workspace);
    workspace = "other";
    controller.setWorkspace(workspace);
    finish();
    await vi.waitFor(() =>
      expect(
        controller.getSnapshot().some((item) => item.phase === "queued"),
      ).toBe(true),
    );
    expect(calls).toEqual(["first", "first"]);
    controller.setWorkspace("first");
    await vi.waitFor(() => expect(calls).toHaveLength(3));
    expect(calls[2]).toBe("first");
    controller.dispose();
  });
  it("does not begin a queued file in a different workspace or publish a wrong resume record", async () => {
    const request = vi.fn();
    const progress = vi.fn();
    await expect(
      createUploadFile(request, () => "other")(file(), "worker", {
        workspace: "first",
        onProgress: progress,
      }),
    ).rejects.toThrow("Workspace changed");
    expect(request).not.toHaveBeenCalled();
    expect(progress).not.toHaveBeenCalled();
  });
  it("keeps a conversation's files until a message takes them", async () => {
    vi.stubGlobal("crypto", webcrypto);
    let next = 0;
    const uploaded = vi.fn();
    const controller = new UploadController(
      { uploadFile: async () => ({ path: "/worker/" + ++next }) },
      uploaded,
    );
    controller.add([file(), file()], "worker", "work");
    controller.add([file()], "other", "work");
    await vi.waitFor(() =>
      expect(controller.ready("worker", "work")).toHaveLength(2),
    );
    expect(uploaded).toHaveBeenCalledTimes(3);
    const [first] = controller.ready("worker", "work");
    controller.take([first.id]);
    expect(controller.of("worker", "work").map((item) => item.path)).toEqual([
      controller.ready("worker", "work")[0].path,
    ]);
    // Another conversation's files are its own.
    expect(controller.of("other", "work")).toHaveLength(1);
    expect(controller.of("worker", "elsewhere")).toHaveLength(0);
    controller.dispose();
  });
  it("never evicts a file waiting for its message to make room", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const controller = new UploadController({
      uploadFile: async () => ({ path: "/worker/file" }),
    });
    controller.add(
      Array.from({ length: 8 }, () => file()),
      "worker",
      "work",
    );
    await vi.waitFor(() =>
      expect(controller.ready("worker", "work")).toHaveLength(8),
    );
    expect(() => controller.add([file()], "worker", "work")).toThrow(
      "at most 8",
    );
    expect(controller.of("worker", "work")).toHaveLength(8);
    // Another conversation still has room.
    expect(() => controller.add([file()], "another", "work")).not.toThrow();
    controller.take(controller.of("worker", "work").map((item) => item.id));
    expect(controller.of("worker", "work")).toHaveLength(0);
    controller.dispose();
  });
  it("puts a taken-back message's files back, once, with the preview it still holds", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const controller = new UploadController({
      uploadFile: async (file) => ({ path: "/w/" + file.name }),
    });
    const image = new File(["png"], "a.png", {
      type: "image/png",
    }) as unknown as globalThis.File;
    controller.add([image], "worker", "work");
    await vi.waitFor(() =>
      expect(controller.ready("worker", "work")).toHaveLength(1),
    );
    const preview = controller.of("worker", "work")[0].image;
    expect(preview).toMatch(/^blob:/);
    controller.take([controller.of("worker", "work")[0].id]);
    // The sent message's chip can still show it.
    expect(controller.sentImage("/w/a.png")).toBe(preview);
    controller.restore(
      [
        { name: "a.png", path: "/w/a.png" },
        { name: "b.txt", path: "/w/b.txt" },
        { name: "a.png", path: "/w/a.png" },
      ],
      "worker",
      "work",
    );
    const items = controller.of("worker", "work");
    expect(
      items.map((item) => [item.file.name, item.path, item.phase]),
    ).toEqual([
      ["a.png", "/w/a.png", "ready"],
      ["b.txt", "/w/b.txt", "ready"],
    ]);
    expect(items.every((item) => item.restored)).toBe(true);
    expect(items[0].image).toBe(preview);
    expect(controller.sentImage("/w/a.png")).toBeUndefined();
    controller.restore([{ name: "b.txt", path: "/w/b.txt" }], "worker", "work");
    expect(controller.of("worker", "work")).toHaveLength(2);
    controller.dispose();
  });
  it("keeps an already committed file visible after a lost commit acknowledgement is cancelled", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const uploaded = vi.fn();
    const resume = {
      id: "transfer",
      workspace: "work",
      thread: "worker",
      sha256: "a".repeat(64),
      offset: 5,
    };
    const controller = new UploadController(
      {
        uploadFile: async (_file, _thread, options) => {
          options?.onProgress?.({
            phase: "committing",
            loaded: 5,
            total: 5,
            resume,
          });
          throw new Error("lost reply");
        },
        cancelUpload: async () => ({
          committed: true,
          cancelled: false,
          path: "/worker/file",
        }),
      },
      uploaded,
    );
    controller.add([file()], "worker", "work");
    await vi.waitFor(() =>
      expect(controller.getSnapshot()[0].phase).toBe("error"),
    );
    await controller.remove(controller.getSnapshot()[0].id);
    expect(controller.getSnapshot()[0].phase).toBe("ready");
    expect(controller.getSnapshot()[0].error).toContain("stays attached");
    expect(controller.getSnapshot()[0].error).not.toContain("/");
    expect(uploaded).not.toHaveBeenCalled();
    controller.dispose();
  });
  it("resumes a lost chunk acknowledgement by verifying server offset and prefix", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const payload = file();
    let received = "";
    let loseAck = true;
    let saved: UploadResume | undefined;
    const methods: string[] = [];
    const request = async (method: string, params: any) => {
      methods.push(method);
      if (method === "upload-begin") return { received: 0, chunk_size: 2 };
      if (method === "upload-status")
        return {
          received: received.length,
          chunk_size: 2,
          prefix_sha256: await fileDigest(
            new TextEncoder().encode(received).buffer,
          ),
        };
      if (method === "upload-chunk") {
        expect(params.offset).toBe(received.length);
        received += atob(params.data);
        if (loseAck) {
          loseAck = false;
          throw new Error("Connection lost");
        }
        return { received: received.length };
      }
      if (method === "upload-commit")
        return { path: "/remote/.twr-uploads/hello.txt" };
      throw new Error(method);
    };
    const upload = createUploadFile(request, () => "workspace");
    await expect(
      upload(payload, "thread", {
        onProgress: (progress) => {
          saved = progress.resume;
        },
      }),
    ).rejects.toThrow("Connection lost");
    expect(saved?.offset).toBe(0);
    expect(received).toBe("he");
    expect(await upload(payload, "thread", { resume: saved })).toEqual({
      path: "/remote/.twr-uploads/hello.txt",
    });
    expect(received).toBe("hello");
    expect(methods.filter((method) => method === "upload-begin")).toHaveLength(
      1,
    );
    expect(methods).not.toContain("upload-cancel");
  });
  it("refuses changed destinations and mismatching server prefixes", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const sha256 = await fileDigest(await file().arrayBuffer());
    const resume: UploadResume = {
      id: "x",
      thread: "thread",
      workspace: "first",
      sha256,
      offset: 2,
    };
    const request = vi.fn(async () => ({
      received: 2,
      prefix_sha256: "wrong",
    }));
    await expect(
      createUploadFile(request, () => "second")(file(), "thread", { resume }),
    ).rejects.toThrow("Workspace changed");
    expect(request).not.toHaveBeenCalled();
    await expect(
      createUploadFile(request, () => "first")(file(), "thread", { resume }),
    ).rejects.toThrow("do not match");
  });
  it("pauses without cancelling, then removes staged transfers explicitly", async () => {
    vi.stubGlobal("crypto", webcrypto);
    let finish!: () => void;
    const done = new Promise<void>((resolve) => {
      finish = resolve;
    });
    const uploaded = vi.fn();
    const cancelUpload = vi.fn(async () => {});
    const controller = new UploadController(
      {
        cancelUpload,
        uploadFile: async (_file, thread, options) => {
          options?.onProgress?.({
            phase: "uploading",
            loaded: 2,
            total: 5,
            resume: {
              id: "x",
              thread,
              workspace: "w",
              sha256: "hash",
              offset: 2,
            },
          });
          await done;
          if (options?.signal?.aborted)
            throw new DOMException("paused", "AbortError");
          return { path: "/remote/file" };
        },
      },
      uploaded,
    );
    controller.add([file()], "thread", "w");
    const id = controller.getSnapshot()[0].id;
    controller.pause(id);
    finish();
    await vi.waitFor(() =>
      expect(controller.getSnapshot()[0].phase).toBe("paused"),
    );
    expect(cancelUpload).not.toHaveBeenCalled();
    expect(uploaded).not.toHaveBeenCalled();
    await controller.remove(id);
    expect(cancelUpload).toHaveBeenCalledOnce();
    expect(controller.getSnapshot()).toEqual([]);
    controller.dispose();
  });
});
