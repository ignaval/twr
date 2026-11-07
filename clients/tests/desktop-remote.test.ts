import { afterEach, describe, expect, it, vi } from "vitest";
import { createServer, type Server } from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import { DesktopTransport } from "../desktop/remote-transport";
import type { AddressInfo } from "node:net";
const token = "a".repeat(64);
const gateway = "gateway-test";
const cleanup: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const close of cleanup.splice(0)) await close();
});
async function fixture(options: { legacy?: boolean; mismatch?: boolean } = {}) {
  let paired = false;
  let rejectStatus = 0;
  let authenticate = true;
  const headers: string[] = [];
  const sockets: WebSocket[] = [];
  const server: Server = createServer((request, response) => {
    response.setHeader("Content-Type", "application/json");
    if (request.url === "/health")
      response.end(
        JSON.stringify({
          service: "twr-mobile",
          protocol: 1,
          ...(options.legacy ? {} : { gateway_id: gateway }),
        }),
      );
    else if (request.url === "/v1/pair") {
      request.resume();
      request.on("end", () => {
        if (paired) {
          response.statusCode = 400;
          response.end(JSON.stringify({ error: "Code already consumed" }));
        } else {
          paired = true;
          response.end(
            JSON.stringify({
              token,
              scopes: ["read"],
              ...(options.legacy
                ? {}
                : { gateway_id: options.mismatch ? "other-gateway" : gateway }),
            }),
          );
        }
      });
    } else {
      response.statusCode = 404;
      response.end("{}");
    }
  });
  const ws = new WebSocketServer({
    server,
    verifyClient: (_info, done) => {
      if (rejectStatus) done(false, rejectStatus, "Access denied");
      else done(true);
    },
  });
  ws.on("connection", (socket, request) => {
    headers.push(String(request.headers.authorization));
    sockets.push(socket);
    if (authenticate)
      socket.send(JSON.stringify({ type: "authenticated", framing: true }));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  cleanup.push(async () => {
    for (const socket of sockets) socket.terminate();
    await new Promise<void>((resolve) => ws.close(() => resolve()));
    await new Promise<void>((resolve) => server.close(() => resolve()));
  });
  const data = new Map<string, string>();
  const ensureAvailable = vi.fn();
  const vault = {
    ensureAvailable,
    read: async (key: string) => data.get(key) || null,
    write: async (key: string, value: string) => {
      data.set(key, value);
    },
    delete: async (key: string) => {
      data.delete(key);
    },
  };
  const events: any[] = [];
  const native = new DesktopTransport(
    vault,
    (event) => events.push(event),
    true,
  );
  cleanup.unshift(() => native.disconnect());
  return {
    url: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
    native,
    data,
    vault,
    rejectStatus: (status: number) => {
      rejectStatus = status;
    },
    authenticate: (value: boolean) => {
      authenticate = value;
    },
    ensureAvailable,
    events,
    headers,
    sockets,
  };
}
describe("native desktop gateway adapter", () => {
  it("pairs a legacy gateway without an identity and rejects canonical URL duplicates", async () => {
    const f = await fixture({ legacy: true });
    expect(await f.native.check(f.url)).toMatchObject({ gatewayId: "" });
    await f.native.pair({
      url: f.url + "/",
      code: "code",
      name: "Desktop",
      hubName: "Legacy",
    });
    await expect(
      f.native.pair({
        url: f.url,
        code: "code2",
        name: "Desktop",
        hubName: "Legacy",
      }),
    ).rejects.toThrow("already paired");
  });
  it("rejects a known identity mismatch without persisting credentials", async () => {
    const f = await fixture({ mismatch: true });
    await expect(
      f.native.pair({
        url: f.url,
        code: "code",
        name: "Desktop",
        hubName: "Test",
      }),
    ).rejects.toThrow("identity changed");
    expect((await f.native.listHubs()).hubs).toEqual([]);
  });
  it("can reset unreadable saved credentials without decrypting or changing other vault records", async () => {
    const { native, vault, data } = await fixture();
    data.set("remote-hubs", "invalid encrypted payload");
    data.set("unrelated", "keep");
    await expect(native.listHubs()).rejects.toThrow("SAVED_HUBS_UNREADABLE");
    const read = vi
      .spyOn(vault, "read")
      .mockRejectedValue(new Error("key changed"));
    await native.resetSavedHubs();
    expect(data.has("remote-hubs")).toBe(false);
    expect(data.get("unrelated")).toBe("keep");
    read.mockRestore();
    expect(await native.listHubs()).toEqual({ hubs: [] });
  });
  it("keeps tokens in its vault and authenticates sockets without exposing them in metadata", async () => {
    const f = await fixture();
    const hub = await f.native.pair({
      url: f.url,
      code: "one-use",
      name: "Desktop",
      hubName: "Test workstation",
    });
    expect(f.ensureAvailable).toHaveBeenCalledOnce();
    expect(JSON.stringify(hub)).not.toContain(token);
    expect(JSON.stringify(await f.native.listHubs())).not.toContain(token);
    expect(f.data.get("remote-hubs")).toContain(token);
    await f.native.connect({ id: hub.id });
    expect(f.headers).toEqual([`Bearer ${token}`]);
    expect(f.events).toContainEqual({ type: "authenticated", framing: true });
    await expect(
      f.native.pair({
        url: f.url,
        code: "another",
        name: "Desktop",
        hubName: "Again",
      }),
    ).rejects.toThrow("already paired");
    await f.native.removeHub({ id: hub.id });
    expect((await f.native.listHubs()).hubs).toEqual([]);
    await expect(f.native.send({ message: "{}" })).rejects.toThrow("closed");
  });
  it("refuses unavailable secure storage before consuming an invitation", async () => {
    const f = await fixture();
    f.ensureAvailable.mockImplementation(() => {
      throw new Error("Secure storage unavailable");
    });
    await expect(
      f.native.pair({
        url: f.url,
        code: "code",
        name: "Desktop",
        hubName: "Test",
      }),
    ).rejects.toThrow("Secure storage");
    f.ensureAvailable.mockImplementation(() => {});
    await expect(
      f.native.pair({
        url: f.url,
        code: "code",
        name: "Desktop",
        hubName: "Test",
      }),
    ).resolves.toHaveProperty("id");
  });
  it("fences the old socket's late frames after disconnect", async () => {
    const f = await fixture();
    const hub = await f.native.pair({
      url: f.url,
      code: "code",
      name: "Desktop",
      hubName: "Test",
    });
    await f.native.connect({ id: hub.id });
    await f.native.disconnect();
    const before = f.events.length;
    f.sockets[0].send(
      JSON.stringify({ type: "state", secret: "old" }),
      () => {},
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(f.events).toHaveLength(before);
  });
  it.each([401, 403])(
    "marks HTTP %i as revoked access instead of a retryable network loss",
    async (status) => {
      const f = await fixture();
      const hub = await f.native.pair({
        url: f.url,
        code: "one-use",
        name: "Desktop",
        hubName: "Test",
      });
      f.rejectStatus(status);
      await expect(f.native.connect({ id: hub.id })).rejects.toThrow(
        "AUTH_REVOKED",
      );
      expect(f.events.some((event) => event.type === "authenticated")).toBe(
        false,
      );
    },
  );
  it("does not create a socket when credential reading finishes after disconnect", async () => {
    const f = await fixture();
    const hub = await f.native.pair({
      url: f.url,
      code: "one-use",
      name: "Desktop",
      hubName: "Test",
    });
    let release!: (value: string) => void;
    f.vault.read = async () =>
      new Promise<string>((resolve) => {
        release = resolve;
      });
    const connecting = f.native.connect({ id: hub.id });
    await vi.waitFor(() => expect(release).toBeTypeOf("function"));
    await f.native.disconnect();
    release(f.data.get("remote-hubs")!);
    await expect(connecting).rejects.toThrow("superseded");
    expect(f.headers).toEqual([]);
  });
  it("fences a socket disconnected during authentication before selecting another hub", async () => {
    const f = await fixture();
    const hub = await f.native.pair({
      url: f.url,
      code: "one-use",
      name: "Desktop",
      hubName: "Test",
    });
    f.authenticate(false);
    const old = f.native.connect({ id: hub.id }).catch((error) => error);
    await vi.waitFor(() => expect(f.sockets).toHaveLength(1));
    await f.native.disconnect();
    f.authenticate(true);
    await f.native.connect({ id: hub.id });
    expect(await old).toBeInstanceOf(Error);
    f.sockets[0].send(
      JSON.stringify({ type: "authenticated", framing: false }),
      () => {},
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(f.events.filter((event) => event.type === "authenticated")).toEqual([
      { type: "authenticated", framing: true },
    ]);
  });
});
