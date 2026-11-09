import { beforeEach, describe, expect, it, vi } from "vitest";
const fs = vi.hoisted(() => ({
  writeFile: vi.fn(async ({ path }: { path: string }) => ({
    uri: `file:///cache/${path}`,
  })),
  deleteFile: vi.fn(async () => {}),
  rmdir: vi.fn(async () => {}),
  share: vi.fn(async () => ({})),
}));
vi.mock("@capacitor/core", () => ({ registerPlugin: () => ({}) }));
vi.mock("@capacitor/clipboard", () => ({ Clipboard: {} }));
vi.mock("@capacitor/filesystem", () => ({
  Filesystem: {
    writeFile: fs.writeFile,
    deleteFile: fs.deleteFile,
    rmdir: fs.rmdir,
  },
  Directory: { Cache: "CACHE" },
  Encoding: { UTF8: "utf8" },
}));
vi.mock("@capacitor/share", () => ({ Share: { share: fs.share } }));
vi.mock("../mobile/src/offline", () => ({
  secureRead: async () => null,
  secureWrite: async () => {},
  secureDelete: async () => {},
}));
import { RemoteHost, sweepExports } from "../mobile/src/transport";

const hub = { id: "h", name: "Hub", url: "https://hub.invalid", scopes: [] };
const exportText = (name: string, content: string) =>
  new RemoteHost(
    hub,
    () => {},
    async () => null,
  )
    .api()
    .exportText(name, content);

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("navigator", { userAgent: "iPhone" });
});

describe("mobile transcript exports", () => {
  it("writes into the exports folder and deletes the file once the share sheet closes", async () => {
    await exportText("thread.md", "secret transcript");
    expect(fs.writeFile).toHaveBeenCalledWith(
      expect.objectContaining({
        path: "exports/thread.md",
        directory: "CACHE",
        recursive: true,
      }),
    );
    expect(fs.share).toHaveBeenCalledWith({
      title: "thread.md",
      url: "file:///cache/exports/thread.md",
    });
    expect(fs.deleteFile).toHaveBeenCalledWith({
      path: "exports/thread.md",
      directory: "CACHE",
    });
  });
  it("deletes the file when sharing is cancelled", async () => {
    fs.share.mockRejectedValueOnce(new Error("Share canceled"));
    await expect(exportText("thread.md", "x")).rejects.toThrow("canceled");
    expect(fs.deleteFile).toHaveBeenCalledOnce();
  });
  it("keeps the file a minute on Android for the receiving app to read", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("navigator", { userAgent: "Android" });
    try {
      await exportText("thread.md", "x");
      expect(fs.deleteFile).not.toHaveBeenCalled();
      await vi.advanceTimersByTimeAsync(60_000);
      expect(fs.deleteFile).toHaveBeenCalledOnce();
    } finally {
      vi.useRealTimers();
    }
  });
  it("sweeps the whole folder and ignores a missing one", async () => {
    fs.rmdir.mockRejectedValueOnce(new Error("does not exist"));
    await expect(sweepExports()).resolves.toBeUndefined();
    expect(fs.rmdir).toHaveBeenCalledWith({
      path: "exports",
      directory: "CACHE",
      recursive: true,
    });
  });
});
