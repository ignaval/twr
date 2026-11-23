/**
 * Certificate-free phone pairing, the parts every client shares: codes, routes, the
 * version 2 invitation and its link, saved-route merging and route racing.
 * Contract: docs/design/phone-pairing.md; vectors: shared/fixtures/phone-pairing-v2.json.
 * Pure TypeScript: it runs in the renderer, the mobile web layer and Node.
 */

/** The 32 symbols of a pairing code: no `I`, `O`, `0` or `1`. */
export const CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
export const CODE_LENGTH = 12;
/** The port a typed address without one uses. */
export const DEFAULT_PORT = 47321;
export const MAX_INVITATION_BYTES = 2048;
export const MAX_INVITATION_ROUTES = 8;
/** A saved hub keeps at most this many routes… */
export const MAX_SAVED_ROUTES = 16;
/** …and drops one not seen for this long. */
export const ROUTE_TTL = 30 * 86400_000;
/** The remembered route starts at once; the others this much later. */
export const RACE_STAGGER = 250;
export const LINK_PREFIX = "twr://pair#";

export type NetworkType = "wifi" | "cellular" | "other" | "none";

/** `input` without ASCII spaces and `-`, uppercased, if that is exactly a code. */
export function normalizeCode(input: string): string | undefined {
  const code = input
    .replace(/[ -]/g, "")
    .replace(/[a-z]/g, (c) => c.toUpperCase());
  if (code.length !== CODE_LENGTH) return undefined;
  for (const c of code) if (!CODE_ALPHABET.includes(c)) return undefined;
  return code;
}

/** `XXXX-XXXX-XXXX` for a code, or the text unchanged when it isn't one. */
export function displayCode(code: string): string {
  const normalized = normalizeCode(code);
  return normalized
    ? `${normalized.slice(0, 4)}-${normalized.slice(4, 8)}-${normalized.slice(8)}`
    : code;
}

/** The pin in groups of four, for reading it aloud or comparing it by eye. */
export function displayPin(pin: string): string {
  return (pin.match(/.{1,4}/g) || []).join(" ");
}

const B64URL =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/** base64url without padding. */
export function base64UrlEncode(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const chunk = bytes.length - i;
    const n =
      (bytes[i] << 16) |
      ((chunk > 1 ? bytes[i + 1] : 0) << 8) |
      (chunk > 2 ? bytes[i + 2] : 0);
    for (let j = 0; j <= Math.min(chunk, 3); j++)
      out += B64URL[(n >> (18 - 6 * j)) & 63];
  }
  return out;
}

/** Decodes base64url without padding; padding, whitespace, the `+/` alphabet and
 * nonzero trailing bits are refused, as the hub refuses them. */
export function base64UrlDecode(text: string): Uint8Array {
  if (text.length % 4 === 1) throw new Error("Bad base64url length");
  const out: number[] = [];
  let acc = 0;
  let bits = 0;
  for (const c of text) {
    const value = B64URL.indexOf(c);
    if (value < 0) throw new Error("Bad base64url character");
    acc = (acc << 6) | value;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 0xff);
      acc &= (1 << bits) - 1;
    }
  }
  if (acc !== 0) throw new Error("Bad base64url padding bits");
  return Uint8Array.from(out);
}

/** A pin is base64url-nopad of 32 bytes: 43 characters. */
export function isPin(value: unknown): value is string {
  if (typeof value !== "string" || value.length !== 43) return false;
  try {
    return base64UrlDecode(value).length === 32;
  } catch {
    return false;
  }
}

export type Route =
  { kind: "tls"; host: string; port: number } | { kind: "other" };

const IPV4 =
  /^(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)$/;

export function isIPv4(host: string) {
  return IPV4.test(host);
}

/** An IPv6 literal without brackets or a zone, as the platform's URL parser reads it. */
export function isIPv6(host: string) {
  if (!host.includes(":") || !/^[0-9a-fA-F:.]+$/.test(host)) return false;
  try {
    new URL(`http://[${host}]/`);
    return true;
  } catch {
    return false;
  }
}

function isDnsName(name: string) {
  return (
    name.length <= 253 &&
    /[a-zA-Z]/.test(name) &&
    name
      .split(".")
      .every(
        (label) =>
          label.length >= 1 &&
          label.length <= 63 &&
          /^[a-zA-Z0-9-]+$/.test(label) &&
          !label.startsWith("-") &&
          !label.endsWith("-"),
      )
  );
}

/** One route entry, `<kind>:<rest>`. Unknown lowercase kinds are `other`: kept, never
 * dialed. Throws for anything that is not a route. */
export function parseRoute(text: string): Route {
  if (typeof text !== "string" || !text || text.length > 255)
    throw new Error("Bad route");
  const colon = text.indexOf(":");
  if (colon < 0) throw new Error("A route is <kind>:<rest>");
  const kind = text.slice(0, colon);
  if (!/^[a-z]+$/.test(kind)) throw new Error("Bad route kind");
  if (kind !== "tls") return { kind: "other" };
  const rest = text.slice(colon + 1);
  const portAt = rest.lastIndexOf(":");
  if (portAt < 0) throw new Error("A tls route needs a port");
  const portText = rest.slice(portAt + 1);
  if (!/^\d{1,5}$/.test(portText)) throw new Error("Bad port");
  const port = Number(portText);
  if (port < 1 || port > 65535) throw new Error("Bad port");
  let host = rest.slice(0, portAt);
  if (host.startsWith("[")) {
    if (!host.endsWith("]")) throw new Error("Bad IPv6 literal");
    host = host.slice(1, -1);
    if (!isIPv6(host)) throw new Error("Bad IPv6 literal");
  } else if (!isIPv4(host)) {
    if (!isDnsName(host)) throw new Error("Bad host");
    host = host.toLowerCase();
  }
  return { kind: "tls", host, port };
}

/** The canonical text of a tls route (lowercase name, bracketed IPv6). */
export function tlsRoute(host: string, port: number) {
  return `tls:${host.includes(":") ? `[${host}]` : host}:${port}`;
}

/** The route's canonical text, or undefined when it isn't a route. */
export function canonicalRoute(text: string): string | undefined {
  try {
    const route = parseRoute(text);
    return route.kind === "tls" ? tlsRoute(route.host, route.port) : text;
  } catch {
    return undefined;
  }
}

/** `https://<host>:<port>` of a tls route: what a pinned hub shows as its address. */
export function routeUrl(route: string): string {
  const parsed = parseRoute(route);
  if (parsed.kind !== "tls") throw new Error("Not a tls route");
  return `https://${parsed.host.includes(":") ? `[${parsed.host}]` : parsed.host}:${parsed.port}`;
}

/** A route for reading: `192.168.1.20:47321`. */
export function displayRoute(route: string): string {
  return route.startsWith("tls:") ? route.slice(4) : route;
}

function ipv6Words(host: string): number[] | undefined {
  try {
    const normalized = new URL(`http://[${host}]/`).hostname.slice(1, -1);
    const [head, tail] = normalized.split("::");
    const words = (part: string | undefined) =>
      part ? part.split(":").map((w) => parseInt(w, 16)) : [];
    if (tail === undefined) return words(head);
    const front = words(head);
    const back = words(tail);
    return [
      ...front,
      ...Array(8 - front.length - back.length).fill(0),
      ...back,
    ];
  } catch {
    return undefined;
  }
}

/** A private LAN address, which a phone on cellular skips: RFC 1918, fc00::/7 outside
 * the tailnet range (fd7a:115c:a1e0::/48), and `.local` names. */
export function isPrivateRoute(route: string): boolean {
  let parsed: Route;
  try {
    parsed = parseRoute(route);
  } catch {
    return false;
  }
  if (parsed.kind !== "tls") return false;
  const host = parsed.host;
  if (isIPv4(host)) {
    const [a, b] = host.split(".").map(Number);
    return (
      a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168)
    );
  }
  if (host.includes(":")) {
    const words = ipv6Words(host);
    if (!words) return false;
    const tailnet =
      words[0] === 0xfd7a && words[1] === 0x115c && words[2] === 0xa1e0;
    return (words[0] & 0xfe00) === 0xfc00 && !tailnet;
  }
  return host === "local" || host.endsWith(".local");
}

/** What a typed address means: `host`, `host:port`, `[v6]:port` or a bare IPv6 literal.
 * Undefined for anything else (an `https://` URL is certificate mode, not this). */
export function parseAddress(
  input: string,
): { host: string; port: number; route: string } | undefined {
  const text = input.trim();
  if (!text || text.length > 260 || /\s|\//.test(text)) return undefined;
  let host = text;
  let port = DEFAULT_PORT;
  if (isIPv6(text)) host = text;
  else if (text.startsWith("[")) {
    const end = text.indexOf("]");
    if (end < 0) return undefined;
    host = text.slice(1, end);
    const after = text.slice(end + 1);
    if (after) {
      if (!/^:\d{1,5}$/.test(after)) return undefined;
      port = Number(after.slice(1));
    }
  } else if (text.includes(":")) {
    const at = text.lastIndexOf(":");
    if (!/^\d{1,5}$/.test(text.slice(at + 1))) return undefined;
    host = text.slice(0, at);
    port = Number(text.slice(at + 1));
  }
  const route = canonicalRoute(tlsRoute(host, port));
  if (!route) return undefined;
  const parsed = parseRoute(route) as Extract<Route, { kind: "tls" }>;
  return { host: parsed.host, port: parsed.port, route };
}

// ---------------------------------------------------------------- invitations

export interface InvitationV1 {
  version?: 1;
  url: string;
  code: string;
  expiresAt: number;
  scopes?: string[];
}
export interface InvitationV2 {
  version: 2;
  /** The gateway id. */
  id: string;
  name: string;
  pin: string;
  iroh?: string;
  code: string;
  expiresAt: number;
  routes: string[];
  scopes: string[];
}
export type Invitation = InvitationV1 | InvitationV2;

export function isPinnedInvitation(value: Invitation): value is InvitationV2 {
  return value.version === 2;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const SCAN_AGAIN = "Scan a twr pairing invitation";
const V1_KEYS = ["type", "version", "url", "code", "expiresAt"];
const V2_KEYS = [
  "type",
  "version",
  "id",
  "name",
  "pin",
  "iroh",
  "code",
  "expiresAt",
  "routes",
  "scopes",
];

function utf8Length(text: string) {
  return new TextEncoder().encode(text).byteLength;
}

/** The invitation JSON a scan or paste carries: the JSON itself or the `twr://pair#` link. */
export function invitationJson(raw: string): string {
  const text = raw.trim();
  if (!text.startsWith(LINK_PREFIX)) return text;
  const encoded = text.slice(LINK_PREFIX.length);
  // Base64 grows by a third; refuse what can't decode to a valid invitation unread.
  if (encoded.length > Math.ceil((MAX_INVITATION_BYTES * 4) / 3))
    throw new Error("This QR code is too large");
  let bytes: Uint8Array;
  try {
    bytes = base64UrlDecode(encoded);
  } catch {
    throw new Error(SCAN_AGAIN);
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new Error(SCAN_AGAIN);
  }
}

/** Is this text meant to be an invitation (JSON or link), right or wrong? */
export function looksLikeInvitation(raw: string) {
  const text = raw.trim();
  return text.startsWith("{") || text.startsWith(LINK_PREFIX);
}

/** Parses version 1 JSON, version 2 JSON or the `twr://pair#` link. */
export function parseAnyInvitation(raw: string): Invitation {
  const json = invitationJson(raw);
  if (utf8Length(json) > MAX_INVITATION_BYTES)
    throw new Error("This QR code is too large");
  let value: any;
  try {
    value = JSON.parse(json);
  } catch {
    throw new Error(SCAN_AGAIN);
  }
  if (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    value.type !== "twr-pair"
  )
    throw new Error(SCAN_AGAIN);
  if (value.version === 1) return parseV1(value);
  if (value.version === 2) return parseV2(value);
  throw new Error(
    "This invitation needs a newer version of twr. Update the app and scan it again.",
  );
}

function parseV1(value: any): InvitationV1 {
  if (
    typeof value.url !== "string" ||
    typeof value.code !== "string" ||
    !/^[a-zA-Z0-9-]{4,128}$/.test(value.code) ||
    !Number.isSafeInteger(value.expiresAt)
  )
    throw new Error(SCAN_AGAIN);
  let url: URL;
  try {
    url = new URL(value.url);
  } catch {
    throw new Error("The invitation must use an HTTPS hub URL");
  }
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    !["", "/"].includes(url.pathname)
  )
    throw new Error("The invitation must use an HTTPS hub URL");
  // The hub enforces the five-minute lifetime. Device clocks may disagree.
  if (value.expiresAt <= 0) throw new Error("Invalid invitation expiry");
  if (Object.keys(value).some((key) => !V1_KEYS.includes(key)))
    throw new Error("Unexpected invitation data");
  return { url: url.origin, code: value.code, expiresAt: value.expiresAt };
}

function parseV2(value: any): InvitationV2 {
  if (Object.keys(value).some((key) => !V2_KEYS.includes(key)))
    throw new Error("Unexpected invitation data");
  if (typeof value.id !== "string" || !UUID.test(value.id))
    throw new Error("The invitation has no valid hub identity");
  if (typeof value.name !== "string" || [...value.name].length > 80)
    throw new Error("The invitation's hub name is invalid");
  if (!isPin(value.pin)) throw new Error("The invitation's hub key is invalid");
  if (
    value.iroh !== undefined &&
    (typeof value.iroh !== "string" || !value.iroh || value.iroh.length > 128)
  )
    throw new Error("The invitation's iroh endpoint is invalid");
  if (
    typeof value.code !== "string" ||
    normalizeCode(value.code) !== value.code
  )
    throw new Error("The invitation's code is invalid");
  if (!Number.isSafeInteger(value.expiresAt) || value.expiresAt <= 0)
    throw new Error("Invalid invitation expiry");
  if (
    !Array.isArray(value.routes) ||
    value.routes.length < 1 ||
    value.routes.length > MAX_INVITATION_ROUTES
  )
    throw new Error("The invitation has no usable address");
  const routes: string[] = [];
  for (const route of value.routes) {
    const canonical = typeof route === "string" && canonicalRoute(route);
    if (!canonical) throw new Error("The invitation has an invalid address");
    routes.push(canonical);
  }
  const scopes = value.scopes ?? [];
  if (
    !Array.isArray(scopes) ||
    scopes.length > 16 ||
    scopes.some((scope) => typeof scope !== "string" || scope.length > 32)
  )
    throw new Error("The invitation's access list is invalid");
  return {
    version: 2,
    id: value.id,
    name: value.name,
    pin: value.pin,
    ...(value.iroh !== undefined ? { iroh: value.iroh } : {}),
    code: value.code,
    expiresAt: value.expiresAt,
    routes,
    scopes,
  };
}

/** The invitation JSON, in the hub's field order. */
export function encodeInvitationJson(value: Invitation): string {
  if (!isPinnedInvitation(value))
    return JSON.stringify({
      type: "twr-pair",
      version: 1,
      url: value.url,
      code: value.code,
      expiresAt: value.expiresAt,
    });
  return JSON.stringify({
    type: "twr-pair",
    version: 2,
    id: value.id,
    name: value.name,
    pin: value.pin,
    ...(value.iroh !== undefined ? { iroh: value.iroh } : {}),
    code: value.code,
    expiresAt: value.expiresAt,
    routes: value.routes,
    scopes: value.scopes,
  });
}

/** `twr://pair#<base64url(JSON)>`: the secret travels in the fragment. */
export function invitationLink(value: InvitationV2): string {
  return (
    LINK_PREFIX +
    base64UrlEncode(new TextEncoder().encode(encodeInvitationJson(value)))
  );
}

// ---------------------------------------------------------------- saved routes

export interface SeenRoute {
  route: string;
  /** Milliseconds since the epoch the route was last offered or connected. */
  seen: number;
}

/** Merges `incoming` (just seen) into `saved`: canonical, deduplicated, routes unseen
 * for 30 days dropped, at most 16 kept. The incoming routes come first in their own
 * order, then the others, the most recently seen first. Non-routes are ignored. */
export function mergeRoutes(
  saved: SeenRoute[],
  incoming: string[],
  now = Date.now(),
): SeenRoute[] {
  const merged = new Map<string, SeenRoute>();
  for (const route of incoming.slice(0, 64)) {
    const canonical = typeof route === "string" && canonicalRoute(route);
    if (canonical && !merged.has(canonical))
      merged.set(canonical, { route: canonical, seen: now });
  }
  const older = saved
    .map((entry) => ({ ...entry, route: canonicalRoute(entry.route) }))
    .filter(
      (entry): entry is SeenRoute =>
        !!entry.route &&
        !merged.has(entry.route) &&
        Number.isFinite(entry.seen) &&
        now - entry.seen <= ROUTE_TTL,
    )
    .sort((a, b) => b.seen - a.seen);
  for (const entry of older)
    if (!merged.has(entry.route)) merged.set(entry.route, entry);
  return [...merged.values()].slice(0, MAX_SAVED_ROUTES);
}

// ---------------------------------------------------------------- racing

/** Which routes to dial, in order: the remembered route for this network first (started
 * at once), then the other tls routes (started RACE_STAGGER later). Unknown route kinds
 * are skipped; on cellular, so are private LAN addresses, unless they are all there is (a
 * VPN over cellular reports as cellular and may reach them). */
export function racePlan(
  routes: string[],
  remembered: string | undefined,
  network: NetworkType,
): { first?: string; rest: string[] } {
  const tls = [
    ...new Set(routes.map((r) => canonicalRoute(r)).filter(Boolean)),
  ].filter((route) => route!.startsWith("tls:")) as string[];
  const reachable =
    network === "cellular"
      ? tls.filter((route) => !isPrivateRoute(route))
      : tls;
  const dialable = reachable.length ? reachable : tls;
  const first =
    remembered && dialable.includes(canonicalRoute(remembered) || "")
      ? canonicalRoute(remembered)
      : undefined;
  return { first, rest: dialable.filter((route) => route !== first) };
}

export class RouteError extends Error {
  constructor(
    readonly route: string,
    readonly reason: string,
    readonly mismatch = false,
  ) {
    super(
      `${mismatch ? "PIN_MISMATCH: " : ""}${displayRoute(route)}: ${reason}`,
    );
  }
}

/**
 * Races `plan`: the first route at once, the rest after `stagger` (or as soon as the first
 * fails). The first attempt to resolve wins and the others are aborted. When all fail, the
 * error names each route and its reason; a pin mismatch keeps `PIN_MISMATCH` in it.
 */
export async function raceRoutes<T>(
  plan: { first?: string; rest: string[] },
  attempt: (route: string, signal: AbortSignal) => Promise<T>,
  options: { stagger?: number; signal?: AbortSignal; hubName?: string } = {},
): Promise<{ route: string; value: T }> {
  const routes = [...(plan.first ? [plan.first] : []), ...plan.rest];
  if (!routes.length)
    throw new Error(
      "This hub has no address this device can try on this network. Pair it again on a network that reaches it.",
    );
  const stagger = options.stagger ?? RACE_STAGGER;
  return new Promise((resolve, reject) => {
    const controllers = new Map<string, AbortController>();
    const failures: { route: string; error: unknown }[] = [];
    let settled = false;
    let started = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const finish = (error?: Error, value?: { route: string; value: T }) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      for (const [route, controller] of controllers)
        if (route !== value?.route) controller.abort();
      if (error) reject(error);
      else resolve(value!);
    };
    const onAbort = () =>
      finish(new Error("Connection was superseded"), undefined);
    if (options.signal?.aborted) return onAbort();
    options.signal?.addEventListener("abort", onAbort);
    const start = (route: string) => {
      started++;
      const controller = new AbortController();
      controllers.set(route, controller);
      Promise.resolve()
        .then(() => attempt(route, controller.signal))
        .then(
          (value) => finish(undefined, { route, value }),
          (error) => {
            if (settled) return;
            failures.push({ route, error });
            if (started < routes.length && failures.length === started)
              startRest();
            else if (failures.length === routes.length)
              finish(raceFailure(failures, options.hubName));
          },
        );
    };
    let restStarted = false;
    const startRest = () => {
      if (restStarted || settled) return;
      restStarted = true;
      clearTimeout(timer);
      for (const route of plan.rest) start(route);
    };
    if (plan.first) {
      start(plan.first);
      if (plan.rest.length) timer = setTimeout(startRest, stagger);
    } else startRest();
  });
}

function raceFailure(
  failures: { route: string; error: unknown }[],
  hubName?: string,
): Error {
  const reasons = failures.map(
    ({ route, error }) =>
      `${displayRoute(route)}: ${
        error instanceof RouteError
          ? error.reason
          : (error instanceof Error ? error.message : String(error)).replace(
              /^PIN_MISMATCH:\s*/,
              "",
            )
      }`,
  );
  const mismatch = failures.some(
    ({ error }) =>
      (error instanceof RouteError && error.mismatch) ||
      String(error).includes("PIN_MISMATCH"),
  );
  const prefix = mismatch ? "PIN_MISMATCH: " : "";
  const subject = hubName
    ? `${hubName} could not be reached`
    : "No route reached the hub";
  return new Error(`${prefix}${subject}. ${reasons.join("; ")}.`);
}
