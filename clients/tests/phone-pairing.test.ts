import { afterEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { argon2id } from "@noble/hashes/argon2.js";
import {
  MAX_SAVED_ROUTES,
  ROUTE_TTL,
  RouteError,
  base64UrlDecode,
  base64UrlEncode,
  displayCode,
  displayPin,
  encodeInvitationJson,
  invitationLink,
  isPrivateRoute,
  mergeRoutes,
  normalizeCode,
  parseAddress,
  parseAnyInvitation,
  parseRoute,
  racePlan,
  raceRoutes,
  type InvitationV2,
} from "../shared/phone-pairing";
import { invitationText, parseInvitation } from "../shared/connections";
import {
  pairErrorText,
  requestFromAddress,
  requestFromInvitation,
} from "../shared/pair-form";
import {
  pinProof,
  proofKey,
  proofMatches,
  stretchCode,
} from "../desktop/pair-proof";

const vectors = JSON.parse(
  readFileSync(
    new URL("../shared/fixtures/phone-pairing-v2.json", import.meta.url),
    "utf8",
  ),
);
const hex = (text: string) => Uint8Array.from(Buffer.from(text, "hex"));

describe("phone pairing vectors: codes", () => {
  it("normalizes case and separators, and refuses everything else", () => {
    for (const [input, output] of vectors.codes.valid)
      expect(normalizeCode(input)).toBe(output);
    for (const input of vectors.codes.invalid)
      expect(normalizeCode(input)).toBeUndefined();
    expect(normalizeCode(vectors.code)).toBe(vectors.code);
    // Only ASCII spaces and hyphens are separators; tabs and underscores are not.
    expect(normalizeCode("ABCD\tEFGH\tJK23")).toBeUndefined();
    expect(displayCode("abcdefghjk23")).toBe("ABCD-EFGH-JK23");
    expect(displayCode("not-a-code")).toBe("not-a-code");
  });
});

describe("phone pairing vectors: routes", () => {
  it("parses the valid routes as the hub does", () => {
    for (const [input, expected] of vectors.routes.valid)
      expect(parseRoute(input)).toEqual(expected);
  });
  it("refuses the invalid ones", () => {
    for (const input of vectors.routes.invalid)
      expect(() => parseRoute(input), input).toThrow();
  });
  it("reads typed addresses with the default port, brackets and bare IPv6", () => {
    expect(parseAddress("192.168.1.20")).toMatchObject({
      route: "tls:192.168.1.20:47321",
    });
    expect(parseAddress(" Box.Local:8443 ")).toMatchObject({
      host: "box.local",
      port: 8443,
      route: "tls:box.local:8443",
    });
    expect(parseAddress("[2001:db8::5]:443")?.route).toBe(
      "tls:[2001:db8::5]:443",
    );
    expect(parseAddress("2001:db8::5")?.route).toBe("tls:[2001:db8::5]:47321");
    for (const bad of [
      "",
      "https://box",
      "box:0",
      "box:70000",
      "-bad-.example",
      "a b",
      "[::1",
      "12345",
    ])
      expect(parseAddress(bad), bad).toBeUndefined();
  });
  it("knows which routes are private LAN addresses", () => {
    for (const route of [
      "tls:192.168.1.20:1",
      "tls:10.0.0.2:1",
      "tls:172.16.4.4:1",
      "tls:[fd00::1]:1",
      "tls:[fc12::1]:1",
      "tls:box.local:1",
    ])
      expect(isPrivateRoute(route), route).toBe(true);
    for (const route of [
      "tls:100.101.102.103:1",
      "tls:[fd7a:115c:a1e0::5]:1",
      "tls:203.0.113.4:1",
      "tls:[2001:db8::5]:1",
      "tls:box.tail1234.ts.net:1",
      "tls:172.32.0.1:1",
      "relay:anything",
    ])
      expect(isPrivateRoute(route), route).toBe(false);
  });
});

describe("phone pairing vectors: invitations", () => {
  const invitation = JSON.parse(vectors.invitation_json);
  it("parses the JSON and the link to the same invitation", () => {
    const fromJson = parseAnyInvitation(vectors.invitation_json);
    const fromLink = parseAnyInvitation(vectors.invitation_link);
    expect(fromLink).toEqual(fromJson);
    expect(fromJson).toMatchObject({
      version: 2,
      id: vectors.gateway_id,
      pin: vectors.pin,
      code: vectors.code,
      routes: invitation.routes,
    });
  });
  it("encodes the hub's exact JSON and link", () => {
    const parsed = parseAnyInvitation(vectors.invitation_json) as InvitationV2;
    expect(encodeInvitationJson(parsed)).toBe(vectors.invitation_json);
    expect(invitationLink(parsed)).toBe(vectors.invitation_link);
    // Copy invitation copies the link for a pinned hub.
    expect(invitationText(parsed)).toBe(vectors.invitation_link);
  });
  it("still reads version 1", () => {
    const v1 = JSON.stringify({
      type: "twr-pair",
      version: 1,
      url: "https://hub.example:47321/",
      code: "abcdef-123456",
      expiresAt: 1,
    });
    expect(parseInvitation(v1)).toEqual({
      url: "https://hub.example:47321",
      code: "abcdef-123456",
      expiresAt: 1,
    });
  });
  it("keeps routes of unknown kinds", () => {
    const parsed = parseAnyInvitation(
      JSON.stringify({
        ...invitation,
        routes: ["relay:x", ...invitation.routes],
      }),
    ) as InvitationV2;
    expect(parsed.routes[0]).toBe("relay:x");
    // …but never dials them.
    expect(racePlan(parsed.routes, undefined, "wifi").rest).not.toContain(
      "relay:x",
    );
  });
  it.each([
    ["an unknown top-level key", { url: "https://x" }],
    ["a wrong type", { type: "other" }],
    ["version 3", { version: 3 }],
    ["no routes", { routes: [] }],
    ["nine routes", { routes: Array(9).fill("tls:1.2.3.4:1") }],
    ["an invalid route", { routes: ["tls:1.2.3.4"] }],
    ["an uppercase route kind", { routes: ["TLS:1.2.3.4:1"] }],
    ["a short pin", { pin: vectors.pin.slice(1) }],
    ["a pin with padding", { pin: vectors.pin.slice(0, 42) + "=" }],
    ["a code with separators", { code: "ABCD-EFGH-JK23" }],
    ["a lowercase code", { code: "abcdefghjk23" }],
    ["an 11-character code", { code: "ABCDEFGHJK2" }],
    ["a gateway id that isn't a UUID", { id: "gateway" }],
    ["a long name", { name: "n".repeat(81) }],
    ["a fractional expiry", { expiresAt: 1.5 }],
    ["string scopes", { scopes: "read" }],
  ])("refuses %s", (_label, change) => {
    expect(() =>
      parseAnyInvitation(JSON.stringify({ ...invitation, ...change })),
    ).toThrow();
  });
  it("refuses a missing required field", () => {
    for (const key of ["id", "name", "pin", "code", "expiresAt", "routes"]) {
      const value = { ...invitation };
      delete value[key];
      expect(() => parseAnyInvitation(JSON.stringify(value)), key).toThrow();
    }
  });
  it("refuses version 1 with version 2 fields and the reverse", () => {
    expect(() =>
      parseAnyInvitation(
        JSON.stringify({
          type: "twr-pair",
          version: 1,
          url: "https://hub.example",
          code: "abcd",
          expiresAt: 1,
          pin: vectors.pin,
        }),
      ),
    ).toThrow("Unexpected");
    expect(() =>
      parseAnyInvitation(JSON.stringify({ ...invitation, version: 1 })),
    ).toThrow();
  });
  it("caps the JSON at 2048 bytes, in either form", () => {
    const big = {
      ...invitation,
      routes: Array.from(
        { length: 8 },
        (_, i) =>
          `tls:${"a".repeat(50)}.${"b".repeat(50)}.${"c".repeat(50)}.${"d".repeat(49)}${i}.example:1`,
      ),
    };
    const json = JSON.stringify(big);
    expect(new TextEncoder().encode(json).length).toBeGreaterThan(2048);
    expect(() => parseAnyInvitation(json)).toThrow("too large");
    const link =
      "twr://pair#" + base64UrlEncode(new TextEncoder().encode(json));
    expect(() => parseAnyInvitation(link)).toThrow("too large");
    // A multi-byte name counts in bytes: under 2048 characters, over 2048 bytes.
    let routeCount = 1;
    const sized = () =>
      JSON.stringify({
        ...invitation,
        name: "€".repeat(80),
        routes: big.routes.slice(0, routeCount),
      });
    while (new TextEncoder().encode(sized()).length <= 2048) routeCount++;
    expect(sized().length).toBeLessThanOrEqual(2048);
    expect(() => parseAnyInvitation(sized())).toThrow("too large");
    routeCount--;
    expect(parseAnyInvitation(sized())).toMatchObject({ name: "€".repeat(80) });
  });
  it("refuses a link that isn't base64url of UTF-8", () => {
    for (const link of [
      "twr://pair#",
      "twr://pair#AA==",
      "twr://pair#A+B/",
      "twr://pair#AB",
      "twr://pair#" + base64UrlEncode(Uint8Array.from([0xff, 0xfe, 0x7b])),
    ])
      expect(() => parseAnyInvitation(link), link).toThrow();
  });
  it("turns an invitation and typed details into pair requests", () => {
    const parsed = parseAnyInvitation(vectors.invitation_link);
    expect(
      requestFromInvitation(parsed, { name: " Phone ", hubName: "box" }),
    ).toEqual({
      pin: vectors.pin,
      gatewayId: vectors.gateway_id,
      routes: invitation.routes,
      iroh: invitation.iroh,
      code: vectors.code,
      name: "Phone",
      hubName: "box",
    });
    expect(
      requestFromAddress("box.local", "abcd efgh jk23", {
        name: "Phone",
        hubName: "box",
      }),
    ).toEqual({
      address: "box.local",
      code: "ABCDEFGHJK23",
      name: "Phone",
      hubName: "box",
    });
    expect(
      requestFromAddress("https://box.example:47321", "legacy-code", {
        name: "Phone",
        hubName: "box",
      }),
    ).toMatchObject({ url: "https://box.example:47321", code: "legacy-code" });
    expect(() =>
      requestFromAddress("box.local", "ABCD-EFGH-JK2", {
        name: "Phone",
        hubName: "box",
      }),
    ).toThrow("12 letters");
  });
  it("says pairing errors in words", () => {
    expect(
      pairErrorText(
        new Error(
          "PIN_MISMATCH: box could not be reached. 192.168.1.20:47321: this is another device, not box.",
        ),
      ),
    ).toMatch(
      /^box could not be reached\. .*another device, not box\. If the hub's key was reset/,
    );
    expect(pairErrorText(new Error("BAD_CODE: The code doesn't match"))).toBe(
      "The code doesn't match",
    );
    expect(displayPin(vectors.pin).split(" ")).toHaveLength(11);
  });
});

describe("phone pairing vectors: the proof", () => {
  it("base64url matches the hub's, padding refused", () => {
    expect(base64UrlEncode(Uint8Array.from([0xfb, 0xff]))).toBe("-_8");
    expect(() => base64UrlDecode("AA==")).toThrow();
    expect(() => base64UrlDecode("AB")).toThrow();
    for (let length = 0; length < 40; length++) {
      const bytes = Uint8Array.from({ length }, (_, i) => (i * 37 + 11) & 255);
      expect(base64UrlDecode(base64UrlEncode(bytes))).toEqual(bytes);
    }
  });
  it("the pin is the SHA-256 of the SPKI", () => {
    expect(
      base64UrlEncode(
        createHash("sha256")
          .update(Buffer.from(vectors.spki_hex, "hex"))
          .digest(),
      ),
    ).toBe(vectors.pin);
  });
  it("@noble/hashes Argon2id gives the hub's stretched key (an independent check of Rust's argon2)", () => {
    const { m_kib, t, p, len, salt } = vectors.argon2id;
    expect([m_kib, t, p, len]).toEqual([65536, 3, 1, 32]);
    const stretched = argon2id(
      new TextEncoder().encode(vectors.code),
      new TextEncoder().encode(salt),
      { m: m_kib, t, p, dkLen: len, version: 0x13 },
    );
    expect(Buffer.from(stretched).toString("hex")).toBe(vectors.stretched_hex);
  }, 30_000);
  it("the bundled worker thread gives the same stretch without blocking the event loop", async () => {
    // What scripts/build-host.mjs inlines into the app: the worker's bundle as source.
    const { build } = await import("esbuild");
    const bundle = await build({
      entryPoints: [
        new URL("../desktop/argon2-worker.ts", import.meta.url).pathname,
      ],
      bundle: true,
      platform: "node",
      format: "cjs",
      write: false,
    });
    const global = globalThis as { __TWR_ARGON2_WORKER__?: string };
    // The worker's answer is the answer: a stand-in worker proves this path is taken.
    global.__TWR_ARGON2_WORKER__ =
      'require("node:worker_threads").parentPort.postMessage({ ok: new Uint8Array([7]) })';
    expect(await stretchCode(vectors.code)).toEqual(Uint8Array.from([7]));
    global.__TWR_ARGON2_WORKER__ = bundle.outputFiles[0].text;
    let ticks = 0;
    const timer = setInterval(() => ticks++, 5);
    try {
      const stretched = await stretchCode(vectors.code);
      expect(Buffer.from(stretched).toString("hex")).toBe(
        vectors.stretched_hex,
      );
      expect(ticks).toBeGreaterThan(10);
    } finally {
      clearInterval(timer);
      delete global.__TWR_ARGON2_WORKER__;
    }
  }, 60_000);
  it("the desktop's stretch, HKDF and HMAC give the vectors' key and proof", async () => {
    // Unbundled, stretchCode runs noble's async variant; bundled, a worker thread.
    const stretched = await stretchCode("abcd-efgh-jk23");
    expect(Buffer.from(stretched).toString("hex")).toBe(vectors.stretched_hex);
    const key = proofKey(stretched);
    expect(key.toString("hex")).toBe(vectors.proof_key_hex);
    const nonce = base64UrlDecode(vectors.nonce_b64url);
    expect(
      base64UrlEncode(pinProof(key, vectors.pin, nonce, vectors.gateway_id)),
    ).toBe(vectors.proof_b64url);
    const proof = base64UrlDecode(vectors.proof_b64url);
    expect(
      proofMatches(key, vectors.pin, nonce, vectors.gateway_id, proof),
    ).toBe(true);
    // Another pin, nonce, gateway or key: no match.
    const other = base64UrlEncode(
      createHash("sha256").update("other").digest(),
    );
    expect(proofMatches(key, other, nonce, vectors.gateway_id, proof)).toBe(
      false,
    );
    expect(
      proofMatches(
        key,
        vectors.pin,
        new Uint8Array(32),
        vectors.gateway_id,
        proof,
      ),
    ).toBe(false);
    expect(proofMatches(key, vectors.pin, nonce, "other", proof)).toBe(false);
    expect(
      proofMatches(
        hex(vectors.stretched_hex),
        vectors.pin,
        nonce,
        vectors.gateway_id,
        proof,
      ),
    ).toBe(false);
    expect(
      proofMatches(key, vectors.pin, nonce, vectors.gateway_id, proof.slice(1)),
    ).toBe(false);
  }, 30_000);
});

describe("saved routes", () => {
  const now = 1_800_000_000_000;
  it("merges reported routes first, deduplicated and canonical", () => {
    const merged = mergeRoutes(
      [
        { route: "tls:192.168.1.20:47321", seen: now - 1000 },
        { route: "tls:old.example:1", seen: now - 5000 },
      ],
      [
        "tls:Box.Example:2",
        "tls:192.168.1.20:47321",
        "tls:box.example:2",
        "junk",
      ],
      now,
    );
    expect(merged).toEqual([
      { route: "tls:box.example:2", seen: now },
      { route: "tls:192.168.1.20:47321", seen: now },
      { route: "tls:old.example:1", seen: now - 5000 },
    ]);
  });
  it("drops routes unseen for 30 days and keeps at most 16", () => {
    expect(
      mergeRoutes(
        [
          { route: "tls:stale.example:1", seen: now - ROUTE_TTL - 1 },
          { route: "tls:fresh.example:1", seen: now - ROUTE_TTL },
        ],
        [],
        now,
      ).map((entry) => entry.route),
    ).toEqual(["tls:fresh.example:1"]);
    const saved = Array.from({ length: 12 }, (_, i) => ({
      route: `tls:saved${i}.example:1`,
      seen: now - i * 1000,
    }));
    const reported = Array.from(
      { length: 8 },
      (_, i) => `tls:new${i}.example:1`,
    );
    const merged = mergeRoutes(saved, reported, now);
    expect(merged).toHaveLength(MAX_SAVED_ROUTES);
    expect(merged.slice(0, 8).map((entry) => entry.route)).toEqual(reported);
    // The oldest saved ones go first.
    expect(merged.at(-1)?.route).toBe("tls:saved7.example:1");
  });
  it("keeps unknown route kinds", () => {
    expect(mergeRoutes([], ["relay:x"], now)).toEqual([
      { route: "relay:x", seen: now },
    ]);
  });
});

describe("racing routes", () => {
  afterEach(() => {
    vi.useRealTimers();
  });
  const routes = [
    "tls:203.0.113.4:47321",
    "tls:192.168.1.20:47321",
    "tls:box.local:47321",
    "tls:100.64.1.2:47321",
    "relay:phase-2",
  ];
  it("plans the remembered route first and skips private addresses on cellular", () => {
    expect(racePlan(routes, "tls:100.64.1.2:47321", "wifi")).toEqual({
      first: "tls:100.64.1.2:47321",
      rest: [
        "tls:203.0.113.4:47321",
        "tls:192.168.1.20:47321",
        "tls:box.local:47321",
      ],
    });
    expect(racePlan(routes, "tls:192.168.1.20:47321", "cellular")).toEqual({
      first: undefined,
      rest: ["tls:203.0.113.4:47321", "tls:100.64.1.2:47321"],
    });
    // A remembered route that is no longer saved is not dialed.
    expect(
      racePlan(routes, "tls:gone.example:1", "other").first,
    ).toBeUndefined();
  });
  it("starts the remembered route at once and the others 250 ms later", async () => {
    vi.useFakeTimers();
    const started: [string, number][] = [];
    const t0 = Date.now();
    const aborted: string[] = [];
    const race = raceRoutes(
      racePlan(routes, "tls:100.64.1.2:47321", "wifi"),
      (route, signal) =>
        new Promise<string>((resolve) => {
          started.push([route, Date.now() - t0]);
          signal.addEventListener("abort", () => aborted.push(route));
          if (route === "tls:192.168.1.20:47321")
            setTimeout(() => resolve("lan"), 100);
        }),
    );
    await vi.advanceTimersByTimeAsync(249);
    expect(started).toEqual([["tls:100.64.1.2:47321", 0]]);
    await vi.advanceTimersByTimeAsync(1);
    expect(started.map(([route]) => route)).toEqual([
      "tls:100.64.1.2:47321",
      "tls:203.0.113.4:47321",
      "tls:192.168.1.20:47321",
      "tls:box.local:47321",
    ]);
    expect(started.slice(1).every(([, at]) => at === 250)).toBe(true);
    await vi.advanceTimersByTimeAsync(100);
    await expect(race).resolves.toEqual({
      route: "tls:192.168.1.20:47321",
      value: "lan",
    });
    // The losers are cancelled.
    expect(aborted.sort()).toEqual(
      [
        "tls:100.64.1.2:47321",
        "tls:203.0.113.4:47321",
        "tls:box.local:47321",
      ].sort(),
    );
  });
  it("starts the rest at once when the remembered route fails early", async () => {
    vi.useFakeTimers();
    const started: string[] = [];
    const race = raceRoutes(
      { first: "tls:a.example:1", rest: ["tls:b.example:1"] },
      async (route) => {
        started.push(route);
        if (route === "tls:a.example:1")
          throw new RouteError(route, "unreachable");
        return route;
      },
    );
    await vi.advanceTimersByTimeAsync(0);
    expect(started).toEqual(["tls:a.example:1", "tls:b.example:1"]);
    await expect(race).resolves.toMatchObject({ route: "tls:b.example:1" });
  });
  it("names every route and its reason when all fail, PIN_MISMATCH kept", async () => {
    const error = await raceRoutes(
      { rest: ["tls:a.example:1", "tls:b.example:1"] },
      async (route) => {
        throw route === "tls:a.example:1"
          ? new RouteError(route, "this is another device, not box", true)
          : new RouteError(route, "timed out");
      },
      { hubName: "box" },
    ).catch((e) => e);
    expect(String(error)).toMatch(
      /^Error: PIN_MISMATCH: box could not be reached\./,
    );
    expect(String(error)).toContain(
      "a.example:1: this is another device, not box",
    );
    expect(String(error)).toContain("b.example:1: timed out");
    expect(String(error).match(/PIN_MISMATCH/g)).toHaveLength(1);
  });
  it("refuses a plan with nothing to dial", async () => {
    await expect(raceRoutes({ rest: [] }, async () => 1)).rejects.toThrow(
      "no address",
    );
  });
});
