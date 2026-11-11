import { afterEach, describe, expect, it, vi } from "vitest";
import {
  aggregateInboxSources,
  cachedInboxSources,
  INBOX_CACHE_TTL,
  withInboxCache,
} from "../shared/inbox-cache";
import { RemoteHost, type NativeTransport } from "../shared/remote";
import type { DesktopAPI, InboxSource } from "../shared/types";

const now = 2_000_000_000_000;
const hubs = [
  { id: "hub-a", name: "Studio" },
  { id: "hub-b", name: "Build box" },
];
function record(workspaces: Record<string, number>, marker = "") {
  return JSON.stringify({
    version: 1,
    drafts: { hidden: "secret draft" },
    outbox: [{ token: "secret queued message" }],
    credentials: "secret root",
    workspaces: Object.fromEntries(
      Object.entries(workspaces).map(([workspace, savedAt]) => [
        workspace,
        {
          savedAt,
          transcripts: { hidden: "secret transcript" },
          view: {
            attention: [
              {
                attn_id: `${marker}${workspace}-request`,
                revision: 3,
                thread: `${workspace}-thread`,
                reason: "permission",
                summary: "Review build",
                options: ["Run"],
                context: { token: "secret context" },
                note: "secret note",
                token: "secret attention",
              },
            ],
            threads: [
              {
                id: `${workspace}-thread`,
                name: `${marker}${workspace} work`,
                runtime: "codex",
                box_name: "local",
                mode: "review",
                status: "idle",
                accent: "#aabbcc",
                is_orchestrator: false,
                credentials: "secret thread",
                repo: "/private/token",
                token: "secret token",
              },
            ],
          },
        },
      ]),
    ),
  });
}
function live(workspace: string): InboxSource {
  return {
    hubId: "hub-a",
    hubName: "Studio",
    workspace,
    attention: [],
    threads: [],
    cached: false,
  };
}
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
describe("saved multi-hub Inbox summaries", () => {
  it("shows an incomplete scan when live discovery fails without a saved summary", async () => {
    const sources = await aggregateInboxSources({
      listHubs: async () => ({ hubs: [] }),
      live: async () => {
        throw new Error("workspace listing failed");
      },
    });
    expect(sources).toHaveLength(1);
    expect(sources[0]).toMatchObject({
      scanError: true,
      attention: [],
      threads: [],
    });
    expect(sources[0].error).toContain("Some requests may be missing");
  });
  it("keeps two saved hubs and distinct workspaces, with original labels and timestamps", async () => {
    const read = vi.fn(async (key: string) =>
      key === "offline:hub-a"
        ? record({ default: now - 1000, research: now - 2000 })
        : record({ builds: now - 3000 }),
    );
    const sources = await aggregateInboxSources({
      now,
      listHubs: async () => ({ hubs }),
      secureRead: read,
      live: async () => [live("default")],
    });
    expect(
      sources.map((source) => [
        source.hubName,
        source.workspace,
        source.cached,
        source.updatedAt,
      ]),
    ).toEqual([
      ["Studio", "default", false, undefined],
      ["Studio", "research", true, now - 2000],
      ["Build box", "builds", true, now - 3000],
    ]);
    expect(read.mock.calls.map(([key]) => key).sort()).toEqual([
      "offline:hub-a",
      "offline:hub-b",
    ]);
    expect(sources[2].attention[0]).toMatchObject({
      summary: "Review build",
      options: [],
      incomplete: true,
    });
    expect(JSON.stringify(sources)).not.toMatch(
      /secret|credentials|transcripts|outbox|drafts|\/private\/token/,
    );
  });
  it("expires summaries after seven days and refuses future timestamps and oversized records", () => {
    const sources = cachedInboxSources(
      record({
        fresh: now - INBOX_CACHE_TTL + 1,
        expired: now - INBOX_CACHE_TTL,
        future: now + 1,
      }),
      hubs[0],
      now,
    );
    expect(sources.map((source) => source.workspace)).toEqual(["fresh"]);
    expect(sources[0].updatedAt).toBe(now - INBOX_CACHE_TTL + 1);
    expect(
      cachedInboxSources("x".repeat(6 * 1024 * 1024 + 1), hubs[0], now),
    ).toEqual([]);
  });
  it("keeps other caches when one keyring read fails or never resolves, with at most three reads active", async () => {
    vi.useFakeTimers();
    let active = 0;
    let maximum = 0;
    const all = [
      { id: "hanging", name: "Unavailable" },
      { id: "broken", name: "Broken" },
      ...Array.from({ length: 8 }, (_, i) => ({
        id: `healthy-${i}`,
        name: `Healthy ${i}`,
      })),
    ];
    const read = vi.fn(async (key: string) => {
      maximum = Math.max(maximum, ++active);
      try {
        if (key === "offline:hanging")
          return await new Promise<string>(() => {});
        if (key === "offline:broken") throw new Error("Keyring locked");
        await Promise.resolve();
        return record({ work: now - 1 });
      } finally {
        active--;
      }
    });
    const pending = aggregateInboxSources({
      now,
      listHubs: async () => ({ hubs: all }),
      secureRead: read,
      live: async () => {
        throw new Error("Disconnected");
      },
    });
    await vi.advanceTimersByTimeAsync(1500);
    const sources = await pending;
    expect(sources.filter((source) => !source.scanError)).toHaveLength(8);
    expect(
      sources
        .filter((source) => !source.scanError)
        .every((source) => source.cached),
    ).toBe(true);
    expect(sources.some((source) => source.scanError && source.error)).toBe(
      true,
    );
    expect(maximum).toBeLessThanOrEqual(3);
    expect(JSON.stringify(sources)).not.toContain("Keyring");
    const again = aggregateInboxSources({
      now,
      listHubs: async () => ({ hubs: all }),
      secureRead: read,
      live: async () => [],
    });
    await vi.advanceTimersByTimeAsync(1500);
    expect((await again).filter((source) => !source.scanError)).toHaveLength(8);
    expect(
      read.mock.calls.filter(([key]) => key === "offline:hanging"),
    ).toHaveLength(1);
    expect(maximum).toBeLessThanOrEqual(3);
  });
  it("retains a useful cache when the live workspace is unavailable", async () => {
    const sources = await aggregateInboxSources({
      now,
      listHubs: async () => ({ hubs: [hubs[0]] }),
      secureRead: async () => record({ default: now - 1000 }),
      live: async () => [{ ...live("default"), error: "Hub is stopped" }],
    });
    expect(sources[0]).toMatchObject({ cached: true, updatedAt: now - 1000 });
    expect(sources[0].attention).toHaveLength(1);
  });
  it("adds This computer only when the desktop adapter requests it", async () => {
    const read = vi.fn(async (key: string) =>
      key === "offline:local" ? record({ localwork: now - 1 }) : null,
    );
    const options = {
      now,
      secureRead: read,
      listHubs: async () => ({ hubs }),
      live: async () => [],
    };
    expect(await aggregateInboxSources(options)).toEqual([]);
    expect(read).not.toHaveBeenCalledWith("offline:local");
    const [source] = await aggregateInboxSources({
      ...options,
      includeLocal: true,
    });
    expect(source).toMatchObject({
      hubId: "local",
      hubName: "This computer",
      workspace: "localwork",
      cached: true,
    });
  });
  it("coalesces local refresh calls and reads other saved hubs without opening a connection", async () => {
    const inboxSources = vi.fn(async () => [live("default")]);
    const secureRead = vi.fn(async () => record({ remote: Date.now() - 1000 }));
    const listHubs = vi.fn(async () => ({ hubs: [hubs[1]] }));
    const api = withInboxCache(
      { inboxSources, secureRead } as unknown as DesktopAPI,
      listHubs,
    );
    const first = api.inboxSources!();
    const second = api.inboxSources!();
    expect(first).toBe(second);
    const sources = await first;
    expect(
      sources.some((source) => source.hubId === "hub-b" && source.cached),
    ).toBe(true);
    expect(inboxSources).toHaveBeenCalledOnce();
    expect(listHubs).toHaveBeenCalledOnce();
  });
  it("exposes other saved hubs through a disconnected RemoteHost without switching or sending responses", async () => {
    const transport = {
      listHubs: vi.fn(async () => ({ hubs })),
      send: vi.fn(async () => {
        throw new Error("Disconnected");
      }),
      connect: vi.fn(),
      disconnect: vi.fn(async () => {}),
    } as unknown as NativeTransport;
    const secureRead = vi.fn(async (key: string) =>
      key === "offline:hub-b" ? record({ builds: Date.now() - 1000 }) : null,
    );
    const host = new RemoteHost(
      { ...hubs[0], url: "https://studio.invalid", scopes: ["read"] },
      () => {},
      async () => null,
      transport,
      {
        platform: "ios",
        secureRead,
        copyText: async () => {},
        openExternal: async () => {},
        reveal: async () => {},
        exportText: async () => {},
      },
    );
    const sources = await host.api().inboxSources!();
    expect(sources.some((source) => source.scanError && source.error)).toBe(
      true,
    );
    expect(sources[0]).toMatchObject({
      hubId: "hub-b",
      hubName: "Build box",
      workspace: "builds",
      cached: true,
    });
    expect(transport.connect).not.toHaveBeenCalled();
    expect(transport.send).toHaveBeenCalledOnce();
    expect(
      JSON.parse((transport.send as any).mock.calls[0][0].message).method,
    ).toBe("inbox-sources");
    expect(secureRead).not.toHaveBeenCalledWith("offline:local");
    await host.stop();
  });
});
