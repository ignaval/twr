import { beforeEach, describe, expect, it, vi } from "vitest";
const fake = vi.hoisted(() => ({
  listener: undefined as any,
  connect: vi.fn(),
  send: vi.fn(),
  disconnect: vi.fn(async () => {}),
  addListener: vi.fn(),
}));
vi.mock("@capacitor/core", () => ({ registerPlugin: () => fake }));
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
});
