import { beforeEach, describe, expect, it, vi } from "vitest";
const fake = vi.hoisted(() => ({
  listener: undefined as any,
  connect: vi.fn(),
  send: vi.fn(),
  disconnect: vi.fn(async () => {}),
  addListener: vi.fn(),
}));
vi.mock("@capacitor/core", () => ({ registerPlugin: () => fake }));
vi.mock("../mobile/src/offline", () => ({
  secureRead: async () => null,
  secureWrite: async () => {},
  secureDelete: async () => {},
}));
import { RemoteHost } from "../mobile/src/transport";
const hub = {
  id: "local",
  name: "Test hub",
  url: "https://hub.invalid",
  scopes: ["read", "operate"],
};
function reply(id: string, result: any) {
  fake.listener({ message: { id, result } });
}
beforeEach(() => {
  vi.clearAllMocks();
  fake.connect.mockResolvedValue(undefined);
  fake.addListener.mockImplementation(async (_name, listener) => {
    fake.listener = listener;
    return { remove: async () => {} };
  });
  fake.send.mockImplementation(async ({ message }) => {
    const call = JSON.parse(message);
    reply(
      call.id,
      call.method === "bootstrap"
        ? { read_methods: ["hub.info", "notify.looking"] }
        : {},
    );
  });
});
describe("mobile reconnect", () => {
  it("clears partial replies and signals transcript recovery after a failed resume probe", async () => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      await host.api().connect("default");
      const events: any[] = [];
      host.api().onEvent((e) => events.push(e));
      fake.listener({
        message: {
          type: "frame-part",
          transfer: "old",
          index: 0,
          count: 2,
          data: "{",
        },
      });
      const original = fake.send.getMockImplementation()!;
      fake.send.mockImplementation(async (payload) => {
        if (JSON.parse(payload.message).method !== "heartbeat")
          await original(payload);
      });
      await vi.advanceTimersByTimeAsync(16000);
      const resumed = host.resume();
      await vi.advanceTimersByTimeAsync(16000);
      await resumed;
      expect(events).toContainEqual(
        expect.objectContaining({ type: "connection", connected: false }),
      );
      expect(fake.connect).toHaveBeenCalledTimes(2);
      // Reusing a transfer ID on the replacement socket must start fresh.
      fake.listener({
        message: {
          type: "frame-part",
          transfer: "old",
          index: 0,
          count: 2,
          data: '{"type":',
        },
      });
      fake.listener({
        message: {
          type: "frame-part",
          transfer: "old",
          index: 1,
          count: 2,
          data: '"state"}',
        },
      });
      expect(events).toContainEqual({ type: "state" });
      expect(fake.disconnect).toHaveBeenCalledTimes(1);
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });

  it("recovers when a fresh native socket silently closes before foregrounding", async () => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      await host.api().connect("default");
      fake.send.mockRejectedValueOnce(new Error("Hub connection is closed"));
      await expect(host.resume()).resolves.toBeUndefined();
      await vi.advanceTimersByTimeAsync(1100);
      expect(fake.disconnect).toHaveBeenCalledTimes(1);
      expect(fake.connect).toHaveBeenCalledTimes(2);
      await expect(host.api().rpc("hub.info")).resolves.toEqual({});
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });

  it("does not discard a progressing transfer when a resume heartbeat is delayed", async () => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      await host.api().connect("default");
      const original = fake.send.getMockImplementation()!;
      fake.send.mockImplementation(async (payload) => {
        if (JSON.parse(payload.message).method !== "heartbeat")
          await original(payload);
      });
      await vi.advanceTimersByTimeAsync(16000);
      const resumed = host.resume();
      setTimeout(
        () =>
          fake.listener({
            message: {
              type: "frame-part",
              transfer: "progress",
              index: 0,
              count: 2,
              data: "{",
            },
          }),
        5000,
      );
      await vi.advanceTimersByTimeAsync(16000);
      await resumed;
      expect(fake.connect).toHaveBeenCalledTimes(1);
      expect(fake.disconnect).not.toHaveBeenCalled();
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });

  it("detects a silent socket without waiting for a long RPC timeout", async () => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      await host.api().connect("default");
      const events: any[] = [];
      host.api().onEvent((event) => events.push(event));
      const original = fake.send.getMockImplementation()!;
      fake.send.mockImplementation(async (payload) => {
        const call = JSON.parse(payload.message);
        if (call.method !== "heartbeat" && call.method !== "config-read")
          await original(payload);
      });
      const pending = host
        .request("config-read", {}, 660000)
        .catch((error) => error);
      await vi.advanceTimersByTimeAsync(61000);
      expect(
        events.some((event) => event.type === "connection" && !event.connected),
      ).toBe(true);
      expect(fake.disconnect).toHaveBeenCalled();
      expect(await pending).toBeInstanceOf(Error);
      expect(fake.connect.mock.calls.length).toBeGreaterThanOrEqual(2);
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });

  it("sends independent heartbeats while a large read reply is pending", async () => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      let readID = "";
      const original = fake.send.getMockImplementation()!;
      fake.send.mockImplementation(async (payload) => {
        const call = JSON.parse(payload.message);
        if (call.method === "config-read") readID = call.id;
        else await original(payload);
      });
      const pending = host.request("config-read", {}, 100000);
      await vi.advanceTimersByTimeAsync(46000);
      const heartbeats = fake.send.mock.calls
        .map(([call]) => JSON.parse(call.message))
        .filter((call) => call.method === "heartbeat");
      expect(heartbeats.length).toBeGreaterThanOrEqual(3);
      expect(fake.connect).toHaveBeenCalledTimes(1);
      reply(readID, { content: "large reply arrived" });
      expect(await pending).toEqual({ content: "large reply arrived" });
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });
  it.each(["x".repeat(2 * 1024 * 1024), "界".repeat(750000)])(
    "rejects oversized UTF-8 mutations without reconnecting",
    async (prompt) => {
      const host = new RemoteHost(
        hub,
        () => {},
        async () => null,
      );
      await host.start();
      const sent = fake.send.mock.calls.length;
      await expect(
        host.api().rpc("thread.send", { thread: "worker", prompt }),
      ).rejects.toThrow("2 MiB");
      expect(fake.send).toHaveBeenCalledTimes(sent);
      expect(fake.connect).toHaveBeenCalledTimes(1);
      expect(await host.api().rpc("hub.info")).toEqual({});
      await host.stop();
    },
  );
  it.each([
    "Gateway refused the connection (403)",
    "Gateway refused the connection (503)",
    "Remote hub connection closed",
  ])("keeps retrying after %s", async (refusal) => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      await host.api().connect("default");
      const events: any[] = [];
      host.api().onEvent((event) => events.push(event));
      fake.connect.mockRejectedValue(new Error(refusal));
      fake.listener({
        message: { type: "transport-closed", connected: false, error: refusal },
      });
      await vi.advanceTimersByTimeAsync(1100);
      await vi.advanceTimersByTimeAsync(2100);
      expect(fake.connect).toHaveBeenCalledTimes(3);
      fake.connect.mockResolvedValue(undefined);
      await vi.advanceTimersByTimeAsync(4100);
      expect(fake.connect).toHaveBeenCalledTimes(4);
      await expect(host.api().rpc("hub.info")).resolves.toEqual({});
      expect(events.some((event) => /AUTH_REVOKED/.test(event.error))).toBe(
        false,
      );
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });
  it("stops retrying once the gateway reports the token revoked (401)", async () => {
    vi.useFakeTimers();
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await host.start();
      await host.api().connect("default");
      const events: any[] = [];
      host.api().onEvent((event) => events.push(event));
      fake.connect.mockRejectedValue(
        new Error(
          "AUTH_REVOKED: This device no longer has access to the hub. Forget it and pair again.",
        ),
      );
      fake.listener({ message: { type: "transport-closed" } });
      await vi.advanceTimersByTimeAsync(1100);
      expect(fake.connect).toHaveBeenCalledTimes(2);
      expect(events).toContainEqual(
        expect.objectContaining({
          type: "connection",
          connected: false,
          error: expect.stringContaining("AUTH_REVOKED"),
        }),
      );
      await vi.advanceTimersByTimeAsync(120_000);
      expect(fake.connect).toHaveBeenCalledTimes(2);
    } finally {
      await host.stop();
      vi.useRealTimers();
    }
  });
  it("resyncs terminal views when the socket returns, even with no further output", async () => {
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    await host.start();
    await host.api().connect("default");
    const events: any[] = [];
    host.api().onEvent((event) => events.push(event));
    fake.listener({ message: { type: "transport-closed" } });
    await host.resume();
    expect(events).toContainEqual({ type: "resync", scope: "terminal" });
    await host.stop();
  });
  it("reuses the original durable ID after a lost reply", async () => {
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    await host.start();
    await host.api().connect("default");
    let original = "";
    let executions = 0;
    fake.send.mockImplementation(async ({ message }) => {
      const call = JSON.parse(message);
      if (call.method === "rpc" && call.params.method === "thread.send") {
        if (!original) {
          original = call.id;
          executions++;
          fake.listener({ message: { type: "transport-closed" } });
        } else {
          expect(call.id).toBe(original);
          reply(call.id, { sent: true });
        }
      } else reply(call.id, {});
    });
    const pending = host.request("rpc", {
      method: "thread.send",
      params: { thread: "worker", prompt: "one message" },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    await host.resume();
    expect(await pending).toEqual({ sent: true });
    expect(executions).toBe(1);
    await host.stop();
  });
  it("keeps calls and the native socket alive during a hub subscription hiccup", async () => {
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    await host.start();
    let id = "";
    fake.send.mockImplementation(async ({ message }) => {
      id = JSON.parse(message).id;
    });
    const pending = host.request("rpc", {
      method: "thread.send",
      params: { thread: "worker", prompt: "keep" },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    fake.listener({ message: { type: "connection", connected: false } });
    reply(id, { sent: true });
    expect(await pending).toEqual({ sent: true });
    expect(fake.connect).toHaveBeenCalledTimes(1);
    await host.stop();
  });
  it("serializes foreground recovery with an explicit workspace switch", async () => {
    const host = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    await host.start();
    await host.api().connect("default");
    const connections: string[] = [];
    fake.send.mockImplementation(async ({ message }) => {
      const call = JSON.parse(message);
      if (call.method === "connect") {
        connections.push(call.params.workspace);
        await new Promise((r) => setTimeout(r, 5));
      }
      reply(call.id, {});
    });
    await Promise.all([host.api().connect("work"), host.resume()]);
    expect(connections).toEqual(["work"]);
    await host.stop();
  });
  it("fences late and queued calls from a hub after another hub takes the native transport", async () => {
    const old = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    await old.start();
    await old.api().connect("default");
    const mutations: string[] = [];
    let release!: () => void;
    fake.send.mockImplementation(async ({ message }) => {
      const call = JSON.parse(message);
      if (call.method === "rpc" && call.params.method === "thread.send") {
        mutations.push(call.params.params.prompt);
        await new Promise<void>((resolve) => {
          release = resolve;
        });
      } else reply(call.id, {});
    });
    const accepted = old
      .api()
      .rpc("thread.send", { thread: "worker", prompt: "already dispatched" })
      .catch((error) => error);
    const queued = old
      .api()
      .rpc("thread.send", {
        thread: "worker",
        prompt: "must not reach the next hub",
      })
      .catch((error) => error);
    await new Promise((resolve) => setTimeout(resolve, 0));
    await old.stop();
    const next = new RemoteHost(
      { ...hub, id: "another-hub" },
      () => {},
      async () => null,
    );
    await next.start();
    release();
    await Promise.all([accepted, queued]);
    await expect(
      old
        .api()
        .rpc("thread.send", { thread: "worker", prompt: "late callback" }),
    ).rejects.toThrow("closed");
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(mutations).toEqual(["already dispatched"]);
    expect(await next.api().rpc("hub.info")).toEqual({});
    await next.stop();
  });
});
