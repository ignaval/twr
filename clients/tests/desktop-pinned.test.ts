import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { TLSSocket, createSecureContext } from "node:tls";
import { createServer as createNetServer, type Socket } from "node:net";
import { createServer as createHttpsServer, type Server } from "node:https";
import { execFileSync } from "node:child_process";
import { createHash, createHmac, randomBytes } from "node:crypto";
import { readFileSync } from "node:fs";
import type { AddressInfo } from "node:net";
import { WebSocketServer, type WebSocket } from "ws";
import { DesktopTransport } from "../desktop/remote-transport";
import { pinnedConnect } from "../desktop/pinned-tls";
import { pinnedCertificates } from "./pinned-certificates";
import { gatewayRequest } from "../desktop/gateway-network";
import {
  base64UrlDecode,
  base64UrlEncode,
  routeUrl,
} from "../shared/phone-pairing";

const vectors = JSON.parse(
  readFileSync(
    new URL("../shared/fixtures/phone-pairing-v2.json", import.meta.url),
    "utf8",
  ),
);
const token = "b".repeat(64);
const GATEWAY = vectors.gateway_id as string;
let tls: ReturnType<typeof pinnedCertificates>;
beforeAll(() => {
  tls = pinnedCertificates();
});
const cleanup: (() => Promise<void> | void)[] = [];
afterEach(async () => {
  for (const close of cleanup.splice(0)) await close();
});

/** A TLS listener that records each TCP connection, each completed handshake, and every
 * byte a client wrote after its handshake, decrypted. */
async function recordingServer(identity: "hub" | "other") {
  const received: Buffer[] = [];

  const connections: Socket[] = [];
  const context = createSecureContext({
    key: tls[identity].key,
    cert: tls[identity].cert,
  });
  const server = createNetServer((socket) => {
    connections.push(socket);
    const secure = new TLSSocket(socket, {
      isServer: true,
      secureContext: context,
    });
    secure.on("data", (chunk: Buffer) => received.push(chunk));
    secure.on("error", () => {});
    socket.on("error", () => {});
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  cleanup.push(
    () => connections.forEach((socket) => socket.destroy()),
    () => new Promise<void>((resolve) => server.close(() => resolve())),
  );
  return {
    route: `tls:127.0.0.1:${(server.address() as AddressInfo).port}`,
    received,

    connections,
  };
}

/** A fake pinned gateway: /health, /v1/pair/proof from the vectors' K_proof, /v1/pair
 * and /v1/socket, with what each request carried recorded. */
async function fakeGateway(
  options: {
    identity?: "hub" | "other";
    key?: Buffer;
    open?: boolean;
    gatewayId?: string;
  } = {},
) {
  const identity = options.identity ?? "hub";
  const pin = tls[identity].pin;
  const gatewayId = options.gatewayId ?? GATEWAY;
  const key = options.key ?? Buffer.from(vectors.proof_key_hex, "hex");
  const requests: { method: string; path: string; body: string }[] = [];
  const sockets: WebSocket[] = [];
  let routes: string[] = [];
  let open = options.open ?? true;
  const server: Server = createHttpsServer(
    { key: tls[identity].key, cert: tls[identity].cert },
    (request, response) => {
      let body = "";
      request.on("data", (chunk) => (body += chunk));
      request.on("end", () => {
        const url = new URL(request.url || "/", "https://gateway.invalid");
        requests.push({
          method: request.method || "",
          path: url.pathname,
          body,
        });
        response.setHeader("Content-Type", "application/json");
        if (url.pathname === "/health")
          return response.end(
            JSON.stringify({
              service: "twr-mobile",
              protocol: 2,
              gateway_id: gatewayId,
              pin,
            }),
          );
        if (url.pathname === "/v1/pair/proof") {
          if (!open) {
            response.statusCode = 404;
            return response.end("{}");
          }
          const nonce = base64UrlDecode(url.searchParams.get("nonce") || "");
          const proof = createHmac("sha256", key)
            .update(Buffer.from("twr-mobile-pin-v2\0"))
            .update(base64UrlDecode(pin))
            .update(nonce)
            .update(gatewayId)
            .digest();
          return response.end(
            JSON.stringify({ proof: base64UrlEncode(proof) }),
          );
        }
        if (url.pathname === "/v1/pair" && request.method === "POST") {
          const { code } = JSON.parse(body);
          if (!open || code !== vectors.code) {
            response.statusCode = 400;
            return response.end(JSON.stringify({ error: "Invalid code" }));
          }
          open = false;
          return response.end(
            JSON.stringify({
              token,
              scopes: ["read", "operate"],
              gateway_id: gatewayId,
            }),
          );
        }
        response.statusCode = 404;
        response.end("{}");
      });
    },
  );
  const ws = new WebSocketServer({
    server,
    verifyClient: (info, done) =>
      done(info.req.headers.authorization === `Bearer ${token}`, 401),
  });
  ws.on("connection", (socket) => {
    sockets.push(socket);
    socket.send(
      JSON.stringify({ type: "authenticated", framing: true, routes }),
    );
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  cleanup.push(async () => {
    for (const socket of sockets) socket.terminate();
    await new Promise<void>((resolve) => ws.close(() => resolve()));
    server.closeAllConnections?.();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  });
  const port = (server.address() as AddressInfo).port;
  return {
    route: `tls:127.0.0.1:${port}`,
    port,
    pin,
    requests,
    sockets,
    setRoutes: (value: string[]) => {
      routes = value;
    },
    reopen: () => {
      open = true;
    },
  };
}

function transport(
  options: ConstructorParameters<typeof DesktopTransport>[3] = {},
) {
  const data = new Map<string, string>();
  const events: any[] = [];
  const native = new DesktopTransport(
    {
      ensureAvailable: () => {},
      read: async (key) => data.get(key) || null,
      write: async (key, value) => {
        data.set(key, value);
      },
      delete: async (key) => {
        data.delete(key);
      },
    },
    (event) => events.push(event),
    false,
    {
      // The vectors' Argon2id output, so most tests skip the 64 MiB KDF.
      stretch: async () =>
        Uint8Array.from(Buffer.from(vectors.stretched_hex, "hex")),
      network: () => "wifi",
      routeTimeout: 3000,
      ...options,
    },
  );
  cleanup.unshift(() => native.disconnect());
  return {
    native,
    data,
    events,
    saved: () => JSON.parse(data.get("remote-hubs") || "{}"),
  };
}

function invitationRequest(routes: string[], pin = tls.hub.pin) {
  return {
    pin,
    gatewayId: GATEWAY,
    routes,
    code: vectors.code,
    name: "Desktop",
    hubName: "box",
  };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 100));

describe("the pin check", () => {
  it("is the SHA-256 of the certificate's SubjectPublicKeyInfo", () => {
    const spki = execFileSync(
      "openssl",
      ["pkey", "-pubout", "-outform", "DER"],
      {
        input: tls.hub.key,
      },
    );
    expect(tls.hub.pin).toBe(
      base64UrlEncode(createHash("sha256").update(spki).digest()),
    );
    expect(tls.hub.pin).toHaveLength(43);
    expect(tls.hub.pin).not.toBe(tls.other.pin);
  });
  it("accepts the pinned key, self-signed, whatever its name", async () => {
    const server = await recordingServer("hub");
    const { socket, pin } = await pinnedConnect(server.route, tls.hub.pin);
    expect(pin).toBe(tls.hub.pin);
    // The recorder sees what a verified socket writes (the control for the next test).
    socket.write("GET / HTTP/1.1\r\n");
    await vi.waitFor(() =>
      expect(Buffer.concat(server.received).toString()).toBe(
        "GET / HTTP/1.1\r\n",
      ),
    );
    socket.destroy();
  });
  it("refuses another key before a single byte is written", async () => {
    const server = await recordingServer("other");
    const error = await pinnedConnect(server.route, tls.hub.pin, {
      hubName: "box",
    }).catch((e) => e);
    expect(String(error)).toMatch(
      /PIN_MISMATCH: 127\.0\.0\.1:\d+: this is another device, not box/,
    );
    await settle();
    // The client got as far as the server's certificate (that is how it saw the other
    // key), then wrote nothing the server could read: the destroy may even drop the
    // client's Finished, so the server never completes the handshake.
    expect(server.connections).toHaveLength(1);
    expect(Buffer.concat(server.received).length).toBe(0);
  });
  it("writes no request line, code or token to another key: pairing", async () => {
    const server = await recordingServer("other");
    const { native, data } = transport();
    const error = await native
      .pair(invitationRequest([server.route]))
      .catch((e) => e);
    expect(String(error)).toContain("PIN_MISMATCH");
    expect(String(error)).toContain("this is another device, not box");
    await settle();
    expect(Buffer.concat(server.received).length).toBe(0);
    expect(data.has("remote-hubs")).toBe(false);
  });
  it("writes no request line or token to another key: connecting", async () => {
    const gateway = await fakeGateway();
    const { native, saved, data } = transport();
    const hub = await native.pair(invitationRequest([gateway.route]));
    // The address now answers with another device's key.
    const impostor = await recordingServer("other");
    const all = saved();
    all[hub.id].routes = [{ route: impostor.route, seen: Date.now() }];
    all[hub.id].last = { wifi: impostor.route };
    data.set("remote-hubs", JSON.stringify(all));
    const error = await native.connect({ id: hub.id }).catch((e) => e);
    expect(String(error)).toContain("PIN_MISMATCH");
    await settle();
    expect(Buffer.concat(impostor.received).length).toBe(0);
    expect(Buffer.concat(impostor.received).toString()).not.toContain(token);
  });
  it("hands the verified socket to http: the request goes over it", async () => {
    const gateway = await fakeGateway();
    const { socket } = await pinnedConnect(gateway.route, gateway.pin);
    const response = await gatewayRequest(routeUrl(gateway.route), "/health", {
      socket,
    });
    expect(response).toMatchObject({ status: 200, data: { pin: gateway.pin } });
  });
});

describe("pairing a scanned invitation", () => {
  it("races the routes with the pin, pairs on the winner and saves pin, routes and last", async () => {
    const gateway = await fakeGateway();
    const unreachable = "tls:127.0.0.1:1";
    const { native, saved } = transport();
    const hub = await native.pair(
      invitationRequest([unreachable, gateway.route, "relay:phase-2"]),
    );
    expect(hub).toMatchObject({
      name: "box",
      pin: gateway.pin,
      gateway_id: GATEWAY,
      route: gateway.route,
      url: `https://127.0.0.1:${gateway.port}`,
      routes: [unreachable, gateway.route, "relay:phase-2"],
    });
    expect(JSON.stringify(hub)).not.toContain(token);
    const record = Object.values(saved())[0] as any;
    expect(record).toMatchObject({
      token,
      pin: gateway.pin,
      last: { wifi: gateway.route },
    });
    expect(gateway.requests.filter((r) => r.path === "/v1/pair")).toHaveLength(
      1,
    );
    // The same hub, again: refused before anything is sent.
    gateway.reopen();
    await expect(
      native.pair(invitationRequest([gateway.route])),
    ).rejects.toThrow("already paired");
    expect(gateway.requests.filter((r) => r.path === "/v1/pair")).toHaveLength(
      1,
    );
  });
  it("refuses a gateway that answers as another hub", async () => {
    const gateway = await fakeGateway({
      gatewayId: "11111111-1111-4111-8111-111111111111",
    });
    const { native } = transport();
    await expect(
      native.pair(invitationRequest([gateway.route])),
    ).rejects.toThrow("another hub identity");
    expect(gateway.requests.some((r) => r.path === "/v1/pair")).toBe(false);
  });
  it("names an unreachable route", async () => {
    const { native } = transport();
    await expect(
      native.pair(invitationRequest(["tls:127.0.0.1:1"])),
    ).rejects.toThrow(/127\.0\.0\.1:1: unreachable/);
  });
});

describe("typed pairing", () => {
  it("checks the gateway's proof with the real Argon2id before sending the code", async () => {
    const gateway = await fakeGateway();
    // No stretch override: the real KDF, as the desktop runs it.
    const { native } = transport({ stretch: undefined });
    const hub = await native.pair({
      address: `127.0.0.1:${gateway.port}`,
      code: "abcd-efgh-jk23",
      name: "Desktop",
      hubName: "box",
    });
    expect(hub).toMatchObject({ pin: gateway.pin, routes: [gateway.route] });
    const paths = gateway.requests.map((r) => r.path);
    expect(paths).toEqual(["/health", "/v1/pair/proof", "/v1/pair"]);
    expect(JSON.parse(gateway.requests[2].body)).toEqual({
      code: vectors.code,
      name: "Desktop",
    });
    // Only the POST carries the code.
    expect(gateway.requests.slice(0, 2).every((r) => !r.body)).toBe(true);
  }, 30_000);
  it("refuses a wrong code without ever sending it", async () => {
    const gateway = await fakeGateway();
    const { native, data } = transport({
      stretch: async () => new Uint8Array(32),
    });
    const error = await native
      .pair({
        address: `127.0.0.1:${gateway.port}`,
        code: "ZZZZ-ZZZZ-ZZZZ",
        name: "Desktop",
        hubName: "box",
      })
      .catch((e) => e);
    expect(String(error)).toMatch(/BAD_CODE: The code doesn't match the hub/);
    expect(gateway.requests.some((r) => r.path === "/v1/pair")).toBe(false);
    expect(data.has("remote-hubs")).toBe(false);
  });
  it("refuses a man in the middle that can't prove its key", async () => {
    // Another key, proving with a key it made up: it doesn't know the code.
    const impostor = await fakeGateway({
      identity: "other",
      key: randomBytes(32),
    });
    const { native } = transport();
    await expect(
      native.pair({
        address: `127.0.0.1:${impostor.port}`,
        code: vectors.code,
        name: "Desktop",
        hubName: "box",
      }),
    ).rejects.toThrow("BAD_CODE");
    expect(impostor.requests.some((r) => r.path === "/v1/pair")).toBe(false);
  });
  it("says when no pairing is open", async () => {
    const gateway = await fakeGateway({ open: false });
    const { native } = transport();
    await expect(
      native.pair({
        address: `127.0.0.1:${gateway.port}`,
        code: vectors.code,
        name: "Desktop",
        hubName: "box",
      }),
    ).rejects.toThrow("No pairing is open");
  });
  it("refuses a malformed code before connecting", async () => {
    const { native } = transport();
    await expect(
      native.pair({
        address: "127.0.0.1:1",
        code: "ABCD",
        name: "D",
        hubName: "b",
      }),
    ).rejects.toThrow("12 letters");
  });
});

describe("connecting to a pinned hub", () => {
  it("opens the socket on a pinned route, merges the hello's routes and remembers the winner", async () => {
    const gateway = await fakeGateway();
    const { native, events, saved } = transport();
    const hub = await native.pair(invitationRequest([gateway.route]));
    gateway.setRoutes([
      "tls:192.168.1.20:47321",
      "tls:box.tail1234.ts.net:47321",
    ]);
    await native.connect({ id: hub.id });
    expect(events).toContainEqual(
      expect.objectContaining({
        type: "authenticated",
        routes: expect.any(Array),
      }),
    );
    // The shared RemoteHost relays the hello's routes; here, directly.
    await native.updateRoutes({
      id: hub.id,
      routes: events.find((e) => e.type === "authenticated").routes,
    });
    const listed = (await native.listHubs()).hubs[0];
    expect(listed.routes).toEqual([
      "tls:192.168.1.20:47321",
      "tls:box.tail1234.ts.net:47321",
      gateway.route,
    ]);
    expect(listed.route).toBe(gateway.route);
    expect((Object.values(saved())[0] as any).last).toEqual({
      wifi: gateway.route,
    });
    await expect(native.send({ message: "{}" })).resolves.toBeUndefined();
  });
  it("tries the remembered route first, then the others", async () => {
    const gateway = await fakeGateway();
    const { native, data, saved } = transport();
    const hub = await native.pair(invitationRequest([gateway.route]));
    const all = saved();
    all[hub.id].routes.unshift({ route: "tls:127.0.0.1:1", seen: Date.now() });
    all[hub.id].last = { wifi: "tls:127.0.0.1:1" };
    data.set("remote-hubs", JSON.stringify(all));
    await native.connect({ id: hub.id });
    await vi.waitFor(() =>
      expect((Object.values(saved())[0] as any).last.wifi).toBe(gateway.route),
    );
  });
  it("never moves certificate-mode credentials to a route", async () => {
    const { native, data } = transport();
    const id = "22222222-2222-4222-8222-222222222222";
    const record = {
      token,
      url: "https://hub.example",
      name: "Cert",
      scopes: [],
    };
    data.set("remote-hubs", JSON.stringify({ [id]: record }));
    await native.updateRoutes({ id, routes: ["tls:192.168.1.20:47321"] });
    expect(JSON.parse(data.get("remote-hubs")!)[id]).toEqual(record);
    expect((await native.listHubs()).hubs[0]).not.toHaveProperty("pin");
  });
  it("cancels the race when disconnected meanwhile", async () => {
    const gateway = await fakeGateway();
    const { native, saved, data } = transport();
    const hub = await native.pair(invitationRequest([gateway.route]));
    const silent = await recordingServer("hub");
    const all = saved();
    // A route that completes TLS but never answers /health.
    all[hub.id].routes = [{ route: silent.route, seen: Date.now() }];
    data.set("remote-hubs", JSON.stringify(all));
    const connecting = native.connect({ id: hub.id }).catch((e) => e);
    await vi.waitFor(() => expect(silent.connections).toHaveLength(1));
    await native.disconnect();
    expect(String(await connecting)).toMatch(/superseded|cancelled/);
  });
});

describe("the desktop's network event", () => {
  it("reports a change of the computer's addresses", () => {
    vi.useFakeTimers();
    try {
      const { native } = transport();
      let state = { type: "other" as const, fingerprint: "en0=192.168.1.4" };
      const seen: any[] = [];
      const stop = native.watchNetwork(
        (value) => seen.push(value),
        1000,
        () => state,
      );
      vi.advanceTimersByTime(3000);
      expect(seen).toEqual([]);
      state = { type: "other", fingerprint: "en0=10.0.0.7" };
      vi.advanceTimersByTime(1000);
      expect(seen).toEqual([{ type: "other" }]);
      stop();
    } finally {
      vi.useRealTimers();
    }
  });
});
