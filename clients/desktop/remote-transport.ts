import WebSocket from "ws";
import { z } from "zod";
import { gatewayRequest, systemCAs } from "./gateway-network";
import type { Hub, PairRequest } from "../shared/remote";

export interface CredentialVault {
  ensureAvailable(): void | Promise<void>;
  read(key: string): Promise<string | null>;
  write(key: string, value: string): Promise<void>;
  delete?(key: string): Promise<void>;
}
const record = z.object({
  token: z.string().regex(/^[a-f0-9]{64}$/),
  url: z.string(),
  name: z.string().max(80),
  scopes: z.array(z.string()),
  gateway_id: z.string().optional(),
});
const records = z.record(z.string().uuid(), record);
type Record = z.infer<typeof record>;
export function gatewayUrl(input: unknown, development = false): string {
  const url = new URL(z.string().min(1).max(2048).parse(input));
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (
    !(
      url.protocol === "https:" ||
      (development && local && url.protocol === "http:")
    ) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    !["", "/"].includes(url.pathname)
  )
    throw new Error(
      "Use an HTTPS hub URL with no path, credentials, query or fragment",
    );
  return url.origin;
}
export const AUTH_REVOKED_MESSAGE =
  "AUTH_REVOKED: This device no longer has access to the hub. Forget it and pair again.";
/** The error for a refused socket upgrade: only 401 is a revocation. */
export function refusal(status: number | undefined) {
  if (status === 401) return AUTH_REVOKED_MESSAGE;
  return `Gateway refused the connection (${status || "unknown"})`;
}
export class DesktopTransport {
  private socket?: WebSocket;
  private generation = 0;
  private activeHub?: string;
  private mutations: Promise<unknown> = Promise.resolve();
  constructor(
    private vault: CredentialVault,
    private emit: (message: unknown) => void,
    private development = false,
  ) {}
  private async all() {
    try {
      const data = await this.vault.read("remote-hubs");
      return data ? records.parse(JSON.parse(data)) : {};
    } catch (cause) {
      throw new Error(
        "SAVED_HUBS_UNREADABLE: Saved connections could not be opened. Unlock your system keyring and retry. If its key was reset, reset saved hubs and pair again.",
        { cause },
      );
    }
  }
  resetSavedHubs() {
    return this.mutate(async () => {
      await this.disconnect();
      if (!this.vault.delete)
        throw new Error("This version cannot reset saved hubs");
      await this.vault.delete("remote-hubs");
    });
  }
  private metadata(id: string, hub: Record): Hub {
    const { token: _token, ...metadata } = hub;
    return { id, ...metadata };
  }
  async listHubs() {
    return {
      hubs: Object.entries(await this.all()).map(([id, hub]) =>
        this.metadata(id, hub),
      ),
    };
  }
  async check(input: unknown, timeout = 15_000) {
    const url = gatewayUrl(input, this.development);
    let response: Awaited<ReturnType<typeof gatewayRequest>>;
    try {
      response = await gatewayRequest(url, "/health", { timeout });
    } catch {
      throw new Error(
        "Could not reach the gateway with a trusted TLS certificate. Check its address, certificate hostname, firewall and network access.",
      );
    }
    if (response.status !== 200)
      throw new Error(`Gateway health check failed (${response.status})`);
    const health = z
      .object({
        service: z.literal("twr-mobile"),
        protocol: z.literal(1),
        gateway_id: z.string().min(1).optional(),
      })
      .parse(response.data);
    return {
      url,
      gatewayId: health.gateway_id || "",
      tls: url.startsWith("https:"),
    };
  }
  pair(input: PairRequest) {
    return this.mutate(async () => {
      await this.vault.ensureAvailable();
      const request = z
        .object({
          url: z.string(),
          code: z.string().trim().min(4).max(128),
          name: z.string().trim().min(1).max(80),
          hubName: z.string().trim().min(1).max(80),
        })
        .strict()
        .parse(input);
      const health = await this.check(request.url);
      const all = await this.all();
      if (
        Object.values(all).some(
          (hub) =>
            (health.gatewayId && hub.gateway_id === health.gatewayId) ||
            gatewayUrl(hub.url, this.development) === health.url,
        )
      )
        throw new Error(
          "This hub is already paired. Forget its saved connection before pairing again.",
        );
      const response = await gatewayRequest(health.url, "/v1/pair", {
        method: "POST",
        body: JSON.stringify({ code: request.code, name: request.name }),
        timeout: 30_000,
      });
      const data = response.data;
      if (response.status !== 200)
        throw new Error(
          typeof data.error === "string"
            ? data.error.slice(0, 240)
            : "Pairing failed. Check the code and hub URL.",
        );
      const hub = record.parse({
        ...data,
        url: health.url,
        name: request.hubName,
      });
      if (health.gatewayId && hub.gateway_id !== health.gatewayId)
        throw new Error("Gateway identity changed during pairing");
      const id = globalThis.crypto.randomUUID();
      all[id] = hub;
      await this.vault.write("remote-hubs", JSON.stringify(all));
      return this.metadata(id, hub);
    });
  }
  private mutate<T>(job: () => Promise<T>): Promise<T> {
    const result = this.mutations.catch(() => {}).then(job);
    this.mutations = result;
    return result;
  }
  async connect(input: { id: string }) {
    const id = z.string().uuid().parse(input.id);
    const generation = ++this.generation;
    this.activeHub = id;
    this.socket?.terminate();
    this.socket = undefined;
    const hub = (await this.all())[id];
    if (!hub) throw new Error("Saved hub not found");
    if (generation !== this.generation)
      throw new Error("Connection was superseded");
    const url =
      gatewayUrl(hub.url, this.development).replace(/^http/, "ws") +
      "/v1/socket";
    await new Promise<void>((resolve, reject) => {
      let authenticated = false;
      let authenticationError: string | undefined;
      const socket = new WebSocket(url, {
        headers: { Authorization: `Bearer ${hub.token}`, "X-Twr-Framing": "1" },
        ca: systemCAs(),
        followRedirects: false,
        handshakeTimeout: 15_000,
        maxPayload: 16 * 1024 * 1024,
      });
      this.socket = socket;
      const timer = setTimeout(() => {
        socket.terminate();
        reject(new Error("Hub authentication timed out"));
      }, 30_000);
      socket.on("message", (data, binary) => {
        if (generation !== this.generation) return;
        try {
          if (binary) throw new Error("Invalid hub response");
          const message = JSON.parse(data.toString());
          if (!message || typeof message !== "object" || Array.isArray(message))
            throw new Error("Invalid hub response");
          if (message.type === "authenticated") {
            authenticated = true;
            clearTimeout(timer);
            resolve();
          }
          this.emit(message);
        } catch {
          socket.terminate();
        }
      });
      socket.on("unexpected-response", (_request, response) => {
        clearTimeout(timer);
        response.resume();
        // Only 401 means the gateway no longer knows this token. A 403 (an
        // Origin the gateway refuses) and 5xx (the hub or its device file is
        // briefly unavailable) are retried like any other failure.
        authenticationError = refusal(response.statusCode);
        reject(new Error(authenticationError));
        socket.terminate();
      });
      socket.on("error", () => {
        clearTimeout(timer);
        if (!authenticated)
          reject(new Error("Could not connect securely to the saved hub"));
      });
      socket.on("close", () => {
        clearTimeout(timer);
        if (!authenticated) reject(new Error("Hub authentication failed"));
        if (generation === this.generation)
          this.emit({
            type: "transport-closed",
            connected: false,
            error: authenticationError || "Remote hub connection closed",
          });
      });
    });
  }
  async send(input: { message: string }) {
    const message = z.string().parse(input.message);
    if (Buffer.byteLength(message) > 2 * 1024 * 1024)
      throw Object.assign(new Error("Request exceeds 2 MiB"), {
        code: "REQUEST_TOO_LARGE",
      });
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN)
      throw new Error("Hub connection is closed");
    await new Promise<void>((resolve, reject) =>
      socket.send(message, (error) =>
        error ? reject(new Error("Hub connection is closed")) : resolve(),
      ),
    );
  }
  async disconnect() {
    ++this.generation;
    this.socket?.terminate();
    this.socket = undefined;
  }
  removeHub(input: { id: string }) {
    return this.mutate(async () => {
      const id = z.string().uuid().parse(input.id);
      const all = await this.all();
      delete all[id];
      if (this.activeHub === id) await this.disconnect();
      await this.vault.write("remote-hubs", JSON.stringify(all));
    });
  }
}
