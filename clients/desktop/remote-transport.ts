import WebSocket from "ws";
import { z } from "zod";
import { randomBytes } from "node:crypto";
import { networkInterfaces } from "node:os";
import type { TLSSocket } from "node:tls";
import { gatewayRequest, systemCAs } from "./gateway-network";
import type { Hub, PairRequest } from "../shared/remote";
import { PAIR_LIMITS, missingPairFields, pairFieldProblem } from "./pairing";
import {
  MAX_INVITATION_ROUTES,
  RouteError,
  base64UrlDecode,
  base64UrlEncode,
  canonicalRoute,
  displayRoute,
  isPin,
  mergeRoutes,
  normalizeCode,
  parseAddress,
  raceRoutes,
  racePlan,
  rememberRoute,
  routeUrl,
  type NetworkType,
  type SeenRoute,
} from "../shared/phone-pairing";
import { pinnedConnect } from "./pinned-tls";
import { proofKey, proofMatches, stretchCode } from "./pair-proof";

export interface CredentialVault {
  ensureAvailable(): void | Promise<void>;
  read(key: string): Promise<string | null>;
  write(key: string, value: string): Promise<void>;
  delete?(key: string): Promise<void>;
}
const pinField = z.string().refine(isPin, "Invalid pin");
const record = z.object({
  token: z.string().regex(/^[a-f0-9]{64}$/),
  url: z.string(),
  name: z.string().max(80),
  scopes: z.array(z.string()),
  gateway_id: z.string().optional(),
  // A pinned hub (docs/design/phone-pairing.md › Saved hub). Certificate-mode records
  // have none of these, and keep system trust.
  pin: pinField.optional(),
  iroh: z.string().max(128).optional(),
  routes: z
    .array(z.object({ route: z.string().max(255), seen: z.number() }))
    .max(64)
    .optional(),
  last: z
    .object({
      wifi: z.string().max(255).optional(),
      cellular: z.string().max(255).optional(),
      other: z.string().max(255).optional(),
    })
    .optional(),
});
const records = z.record(z.string().uuid(), record);
type Record = z.infer<typeof record>;
const health = z.object({
  service: z.literal("twr-mobile"),
  protocol: z.number().int().min(1),
  gateway_id: z.string().min(1).optional(),
  pin: z.string().optional(),
});
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
const ALREADY_PAIRED =
  "This hub is already paired. Forget its saved connection before pairing again.";

/** The desktop's network, for `last`: a computer has no cellular; "none" while every
 * interface but loopback and link-local is down. */
export function desktopNetwork(interfaces = networkInterfaces()): {
  type: NetworkType;
  fingerprint: string;
} {
  const addresses: string[] = [];
  for (const [name, entries] of Object.entries(interfaces))
    for (const entry of entries || []) {
      if (entry.internal || /^fe80:|^169\.254\./i.test(entry.address)) continue;
      // Container and VM bridges come and go without the route out changing.
      if (/^(docker|veth|br-|virbr|vmnet|vboxnet|cni|flannel)/.test(name))
        continue;
      addresses.push(`${name}=${entry.address}`);
    }
  addresses.sort();
  return {
    type: addresses.length ? "other" : "none",
    fingerprint: addresses.join(","),
  };
}

export interface DesktopTransportOptions {
  /** Argon2id of a typed code (pair-proof.ts; injectable for tests). */
  stretch?: (code: string) => Promise<Uint8Array>;
  /** The current network type (desktopNetwork by default). */
  network?: () => NetworkType;
  /** Per-route TLS and request deadline. */
  routeTimeout?: number;
}

export class DesktopTransport {
  private socket?: WebSocket;
  private generation = 0;
  private activeHub?: string;
  private race?: AbortController;
  private mutations: Promise<unknown> = Promise.resolve();
  private networkTimer?: ReturnType<typeof setInterval>;
  constructor(
    private vault: CredentialVault,
    private emit: (message: unknown) => void,
    private development = false,
    private options: DesktopTransportOptions = {},
  ) {}
  private networkType(): NetworkType {
    return this.options.network?.() ?? desktopNetwork().type;
  }
  /** Polls the interfaces and reports a change of the computer's addresses as the
   * contract's `network` event, so a connected pinned hub reconnects at once. */
  watchNetwork(
    emit: (value: { type: NetworkType }) => void,
    interval = 4000,
    read = desktopNetwork,
  ) {
    clearInterval(this.networkTimer);
    let previous = read().fingerprint;
    this.networkTimer = setInterval(() => {
      const now = read();
      if (now.fingerprint === previous) return;
      previous = now.fingerprint;
      emit({ type: now.type });
    }, interval);
    this.networkTimer.unref?.();
    return () => clearInterval(this.networkTimer);
  }
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
    const metadata: Hub = {
      id,
      url: hub.url,
      name: hub.name,
      scopes: hub.scopes,
      ...(hub.gateway_id !== undefined ? { gateway_id: hub.gateway_id } : {}),
    };
    if (!hub.pin) return metadata;
    const route = this.lastRoute(hub) || hub.last?.other;
    return {
      ...metadata,
      pin: hub.pin,
      routes: (hub.routes || []).map((entry) => entry.route),
      ...(route ? { route } : {}),
    };
  }
  /** Where a winning route is remembered: the network type, "other" while offline. */
  private lastKey(): "wifi" | "cellular" | "other" {
    const network = this.networkType();
    return network === "none" ? "other" : network;
  }
  private lastRoute(hub: Record): string | undefined {
    const network = this.networkType();
    return network === "none" ? undefined : hub.last?.[network];
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
    const parsed = health.parse(response.data);
    return {
      url,
      gatewayId: parsed.gateway_id || "",
      tls: url.startsWith("https:"),
    };
  }
  pair(input: PairRequest) {
    return this.mutate(async () => {
      await this.vault.ensureAvailable();
      const fields = (input || {}) as Partial<PairRequest>;
      if (fields.pin !== undefined) return this.pairScanned(fields);
      if (fields.address !== undefined) return this.pairTyped(fields);
      // Keys sent as undefined (an optional field the form left out) aren't extra data.
      return this.pairCertificate(
        Object.fromEntries(
          Object.entries(fields).filter(([, value]) => value !== undefined),
        ) as unknown as PairRequest,
      );
    });
  }
  private async pairCertificate(input: PairRequest) {
    const parsed = z
      .object({
        url: z.string(),
        code: z.string().trim().min(PAIR_LIMITS.codeMin).max(PAIR_LIMITS.code),
        name: z.string().trim().min(1).max(PAIR_LIMITS.name),
        hubName: z.string().trim().min(1).max(PAIR_LIMITS.name),
      })
      .strict()
      .safeParse(input);
    // In words, never zod's issue list.
    if (!parsed.success) {
      const fields = input as Partial<PairRequest> | null;
      const text = (key: keyof PairRequest) =>
        typeof fields?.[key] === "string" ? (fields[key] as string) : "";
      const all = {
        url: text("url"),
        code: text("code"),
        hubName: text("hubName"),
        name: text("name"),
      };
      throw new Error(
        missingPairFields(all) ??
          pairFieldProblem(all) ??
          "The pairing details can't be used. Check them and try again.",
      );
    }
    const request = parsed.data;
    const checked = await this.check(request.url);
    const all = await this.all();
    if (
      Object.values(all).some(
        (hub) =>
          (checked.gatewayId && hub.gateway_id === checked.gatewayId) ||
          gatewayUrl(hub.url, this.development) === checked.url,
      )
    )
      throw new Error(ALREADY_PAIRED);
    const response = await gatewayRequest(checked.url, "/v1/pair", {
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
      url: checked.url,
      name: request.hubName,
    });
    if (checked.gatewayId && hub.gateway_id !== checked.gatewayId)
      throw new Error("Gateway identity changed during pairing");
    return this.save(all, hub);
  }
  private async save(all: { [id: string]: Record }, hub: Record) {
    const id = globalThis.crypto.randomUUID();
    all[id] = hub;
    await this.vault.write("remote-hubs", JSON.stringify(all));
    return this.metadata(id, hub);
  }
  /** Names, as every pairing checks them, in words. */
  private names(fields: Partial<PairRequest>) {
    const name = typeof fields.name === "string" ? fields.name.trim() : "";
    const hubName =
      typeof fields.hubName === "string" ? fields.hubName.trim() : "";
    if (!hubName) throw new Error("Enter a name for the hub.");
    if (!name) throw new Error("Enter a name for this device.");
    if (hubName.length > PAIR_LIMITS.name || name.length > PAIR_LIMITS.name)
      throw new Error(`Names can have at most ${PAIR_LIMITS.name} characters.`);
    return { name, hubName };
  }
  private code(fields: Partial<PairRequest>) {
    const code =
      typeof fields.code === "string" ? normalizeCode(fields.code) : undefined;
    if (!code)
      throw new Error(
        "BAD_CODE: A pairing code has 12 letters and digits, like ABCD-EFGH-JK23. Check it on the hub.",
      );
    return code;
  }
  private refuseDuplicate(
    all: { [id: string]: Record },
    gatewayId: string,
    pin: string,
  ) {
    if (
      Object.values(all).some(
        (hub) => hub.gateway_id === gatewayId || hub.pin === pin,
      )
    )
      throw new Error(ALREADY_PAIRED);
  }
  /** GET /health on a pinned connection; the gateway must be the expected one. */
  private async health(
    route: string,
    socket: TLSSocket,
    expected: { gatewayId?: string; pin: string },
  ) {
    const response = await gatewayRequest(routeUrl(route), "/health", {
      socket,
      timeout: this.options.routeTimeout ?? 10_000,
    }).catch((error) => {
      throw new RouteError(route, `no answer (${error.message})`);
    });
    const parsed = health.safeParse(response.data);
    if (response.status !== 200 || !parsed.success)
      throw new RouteError(route, "not a twr gateway");
    if (parsed.data.pin === undefined)
      throw new RouteError(
        route,
        "this hub uses its own certificate; pair it with its https:// address",
      );
    if (parsed.data.pin !== expected.pin || !parsed.data.gateway_id)
      throw new RouteError(route, "the gateway reported another key");
    if (expected.gatewayId && parsed.data.gateway_id !== expected.gatewayId)
      throw new RouteError(route, "the gateway reported another hub identity");
    return parsed.data.gateway_id;
  }
  /** A route that completes TLS with `pin` and answers /health as `gatewayId`. */
  private async raceHub(
    routes: string[],
    remembered: string | undefined,
    pin: string,
    gatewayId: string,
    hubName: string,
    signal?: AbortSignal,
  ) {
    const plan = racePlan(routes, remembered, this.networkType());
    return raceRoutes(
      plan,
      async (route, abort) => {
        const { socket } = await pinnedConnect(route, pin, {
          signal: abort,
          timeout: this.options.routeTimeout,
          hubName,
        });
        // A loser's /health stops with the race.
        abort.addEventListener("abort", () => socket.destroy(), { once: true });
        await this.health(route, socket, { gatewayId, pin });
      },
      { signal, hubName },
    );
  }
  private async postPair(
    route: string,
    pin: string,
    hubName: string,
    code: string,
    name: string,
  ) {
    const { socket } = await pinnedConnect(route, pin, {
      timeout: this.options.routeTimeout,
      hubName,
    });
    const response = await gatewayRequest(routeUrl(route), "/v1/pair", {
      socket,
      method: "POST",
      body: JSON.stringify({ code, name }),
      timeout: 30_000,
    });
    const data = response.data || {};
    if (response.status !== 200)
      throw new Error(
        typeof data.error === "string"
          ? `${response.status === 401 || response.status === 400 ? "BAD_CODE: " : ""}${data.error.slice(0, 240)}`
          : "Pairing failed. Check the code and create a new invitation if it expired.",
      );
    return data;
  }
  /** A scanned version 2 invitation: race its routes with the pin, then pair. */
  private async pairScanned(fields: Partial<PairRequest>) {
    const { name, hubName } = this.names(fields);
    const code = this.code(fields);
    if (!isPin(fields.pin))
      throw new Error("The invitation's hub key is invalid. Scan it again.");
    const pin = fields.pin;
    const gatewayId = z.string().uuid().safeParse(fields.gatewayId);
    if (!gatewayId.success)
      throw new Error(
        "The invitation has no valid hub identity. Scan it again.",
      );
    const routes = (Array.isArray(fields.routes) ? fields.routes : []).map(
      (route) =>
        typeof route === "string" ? canonicalRoute(route) : undefined,
    );
    if (
      !routes.length ||
      routes.length > MAX_INVITATION_ROUTES ||
      routes.some((route) => !route)
    )
      throw new Error("The invitation's addresses are invalid. Scan it again.");
    const iroh = z.string().min(1).max(128).optional().parse(fields.iroh);
    const all = await this.all();
    this.refuseDuplicate(all, gatewayId.data, pin);
    const winner = await this.raceHub(
      routes as string[],
      undefined,
      pin,
      gatewayId.data,
      hubName,
    );
    const data = await this.postPair(winner.route, pin, hubName, code, name);
    if (data.gateway_id !== undefined && data.gateway_id !== gatewayId.data)
      throw new Error("Gateway identity changed during pairing");
    const now = Date.now();
    const hub = record.parse({
      ...data,
      gateway_id: gatewayId.data,
      url: routeUrl(winner.route),
      name: hubName,
      pin,
      ...(iroh ? { iroh } : {}),
      routes: mergeRoutes([], routes as string[], now),
      last: { [this.lastKey()]: winner.route },
    });
    return this.save(all, hub);
  }
  /**
   * Typed pairing (docs/design/phone-pairing.md › Client trust rule): capture the key
   * the address presents, have the gateway prove it knows the code for that key, and
   * only then send the code.
   */
  private async pairTyped(fields: Partial<PairRequest>) {
    const { name, hubName } = this.names(fields);
    const code = this.code(fields);
    const address =
      typeof fields.address === "string"
        ? parseAddress(fields.address)
        : undefined;
    if (!address)
      throw new Error(
        "Enter the hub's address as a name or IP address, with :port if it isn't 47321.",
      );
    const route = address.route;
    // Argon2id takes about a second: start it while the network work happens.
    const key = (this.options.stretch ?? stretchCode)(code).then(proofKey);
    key.catch(() => {});
    const timeout = this.options.routeTimeout;
    // 1. Connect and capture the pin, untrusted; /health on the same connection.
    const first = await pinnedConnect(route, undefined, { timeout, hubName });
    const pin = first.pin;
    const gatewayId = await this.health(route, first.socket, { pin });
    const all = await this.all();
    this.refuseDuplicate(all, gatewayId, pin);
    // 2. The proof, on a connection presenting the same key.
    const nonce = randomBytes(32);
    const proofSocket = await pinnedConnect(route, pin, { timeout, hubName });
    const served = await gatewayRequest(
      routeUrl(route),
      `/v1/pair/proof?nonce=${base64UrlEncode(nonce)}`,
      { socket: proofSocket.socket, timeout: 40_000 },
    ).catch((error) => {
      throw new RouteError(route, `no answer (${error.message})`);
    });
    if (served.status === 404)
      throw new Error(
        `No pairing is open on ${displayRoute(route)}. Start pairing on the hub, then enter the new code.`,
      );
    if (served.status === 429)
      throw new Error(
        "The hub is refusing pairing attempts for now. Wait a minute and try again.",
      );
    let proof: Uint8Array | undefined;
    try {
      proof =
        served.status === 200 && typeof served.data?.proof === "string"
          ? base64UrlDecode(served.data.proof)
          : undefined;
    } catch {
      proof = undefined;
    }
    if (!proof || proof.length !== 32)
      throw new Error(
        `${displayRoute(route)} answered the pairing check with something else (${served.status}).`,
      );
    // 3. Only a gateway that knows the code can produce this proof for this key.
    if (!proofMatches(await key, pin, nonce, gatewayId, proof))
      throw new Error(
        `BAD_CODE: The code doesn't match the hub at ${displayRoute(route)}. Check the code. If it is right, another device is answering at this address.`,
      );
    // 4. Now the code may go, on a connection that presents the proven key.
    const data = await this.postPair(route, pin, hubName, code, name);
    if (data.gateway_id !== undefined && data.gateway_id !== gatewayId)
      throw new Error("Gateway identity changed during pairing");
    const hub = record.parse({
      ...data,
      gateway_id: gatewayId,
      url: routeUrl(route),
      name: hubName,
      pin,
      routes: mergeRoutes([], [route]),
      last: { [this.lastKey()]: route },
    });
    return this.save(all, hub);
  }
  private mutate<T>(job: () => Promise<T>): Promise<T> {
    const result = this.mutations.catch(() => {}).then(job);
    this.mutations = result;
    return result;
  }
  updateRoutes(input: { id: string; routes: string[] }) {
    return this.mutate(async () => {
      const id = z.string().uuid().parse(input?.id);
      const incoming = z
        .array(z.string().max(255))
        .max(64)
        .parse(input?.routes);
      const all = await this.all();
      const hub = all[id];
      // Certificate-mode credentials never move to a pinned route.
      if (!hub?.pin) return;
      const routes = mergeRoutes(hub.routes || [], incoming);
      if (JSON.stringify(routes) === JSON.stringify(hub.routes)) return;
      all[id] = { ...hub, routes };
      await this.vault.write("remote-hubs", JSON.stringify(all));
    });
  }
  /** The winner becomes `last` for this network type, and is seen now. */
  private remember(id: string, route: string) {
    return this.mutate(async () => {
      const all = await this.all();
      const hub = all[id];
      if (!hub?.pin) return;
      all[id] = {
        ...hub,
        url: routeUrl(route),
        routes: rememberRoute(hub.routes || [], route),
        last: { ...hub.last, [this.lastKey()]: route },
      };
      await this.vault.write("remote-hubs", JSON.stringify(all));
    }).catch(() => {});
  }
  async connect(input: { id: string }) {
    const id = z.string().uuid().parse(input.id);
    const generation = ++this.generation;
    this.activeHub = id;
    this.race?.abort();
    this.race = undefined;
    this.socket?.terminate();
    this.socket = undefined;
    const hub = (await this.all())[id];
    if (!hub) throw new Error("Saved hub not found");
    if (generation !== this.generation)
      throw new Error("Connection was superseded");
    if (!hub.pin) {
      const url =
        gatewayUrl(hub.url, this.development).replace(/^http/, "ws") +
        "/v1/socket";
      return this.open(generation, hub, url, { ca: systemCAs() });
    }
    const race = new AbortController();
    this.race = race;
    const saved = (hub.routes || []).map((entry) => entry.route);
    const winner = await this.raceHub(
      saved,
      this.lastRoute(hub),
      hub.pin,
      hub.gateway_id || "",
      hub.name,
      race.signal,
    );
    if (generation !== this.generation)
      throw new Error("Connection was superseded");
    const { socket } = await pinnedConnect(winner.route, hub.pin, {
      signal: race.signal,
      timeout: this.options.routeTimeout,
      hubName: hub.name,
    });
    if (generation !== this.generation) {
      socket.destroy();
      throw new Error("Connection was superseded");
    }
    void this.remember(id, winner.route);
    return this.open(
      generation,
      hub,
      routeUrl(winner.route).replace(/^https/, "wss") + "/v1/socket",
      { createConnection: () => socket },
    );
  }
  private open(
    generation: number,
    hub: Record,
    url: string,
    transport: { ca?: string[]; createConnection?: () => TLSSocket },
  ) {
    return new Promise<void>((resolve, reject) => {
      let authenticated = false;
      let authenticationError: string | undefined;
      const socket = new WebSocket(url, {
        headers: { Authorization: `Bearer ${hub.token}`, "X-Twr-Framing": "1" },
        ...transport,
        followRedirects: false,
        handshakeTimeout: 15_000,
        maxPayload: 16 * 1024 * 1024,
      } as WebSocket.ClientOptions);
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
    this.race?.abort();
    this.race = undefined;
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
export type { SeenRoute };
