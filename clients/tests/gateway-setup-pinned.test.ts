import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { createServer, type Server } from "node:https";
import { readFileSync, realpathSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AddressInfo } from "node:net";
import {
  GatewaySetup,
  checkLocalPinned,
  gatewayLaunchArgs,
  validateGatewaySettings,
} from "../desktop/gateway-setup";
import type { GatewaySettings } from "../shared/connections";
import { pinnedCertificates } from "./pinned-certificates";

const vectors = JSON.parse(
  readFileSync(
    new URL("../shared/fixtures/phone-pairing-v2.json", import.meta.url),
    "utf8",
  ),
);
const GATEWAY = vectors.gateway_id as string;
const pinned: GatewaySettings = {
  mode: "pinned",
  url: "",
  listen: "0.0.0.0:47321",
  cert: "",
  key: "",
  scopes: "operate",
};
let tls: ReturnType<typeof pinnedCertificates>;
beforeAll(() => {
  tls = pinnedCertificates();
});
const cleanup: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const close of cleanup.splice(0)) await close();
});

function fixture(
  options: {
    running?: "pinned" | "certificate" | false;
    invitation?: any;
  } = {},
) {
  const runningInfo = (mode: "pinned" | "certificate") => ({
    running: true,
    gateway_id: GATEWAY,
    pid: 123,
    instance: "11111111-1111-4111-8111-111111111111",
    version: "0.7.0",
    executable: realpathSync(process.execPath),
    listen: pinned.listen,
    started_at: Date.now() / 1000,
    ...(mode === "pinned"
      ? {
          pin: vectors.pin,
          routes: ["tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321"],
          tls_cert: "",
          tls_key: "",
          certificate_sha256: "",
        }
      : {
          tls_cert: "/etc/cert.pem",
          tls_key: "/etc/key.pem",
          certificate_sha256: "digest",
        }),
  });
  let info: any =
    options.running === false
      ? { running: false, gateway_id: GATEWAY, pin: vectors.pin }
      : runningInfo(options.running ?? "pinned");
  const command = vi.fn(async (args: string[]) => {
    if (args[0] === "--gateway-info") return info;
    if (args[0] === "--cancel-invitation") return { cancelled: true };
    if (args[0] === "--stop-gateway") {
      info = { running: false, gateway_id: GATEWAY };
      return { stopped: true };
    }
    return (
      options.invitation ?? {
        code: vectors.code,
        expires_at: 1763900000,
        scopes: ["read", "operate"],
        gateway_id: GATEWAY,
        invitation: JSON.parse(vectors.invitation_json),
        link: vectors.invitation_link,
      }
    );
  });
  const launch = vi.fn(async () => {
    info = runningInfo("pinned");
    return {
      pid: 123,
      exitCode: () => null,
      signal: () => null,
      stop: async () => {
        info = { running: false, gateway_id: GATEWAY };
      },
    };
  });
  const local = vi.fn(
    async (_settings: GatewaySettings, seen?: { pin?: string }) => ({
      url: "",
      gatewayId: GATEWAY,
      tls: true,
      pin: seen?.pin,
    }),
  );
  const checkPublic = vi.fn();
  const save = vi.fn();
  const setup = new GatewaySetup(
    process.execPath,
    () => ({}),
    join(mkdtempSync(join(tmpdir(), "twr-gateway-log-")), "gateway.log"),
    checkPublic,
    save,
    options.running === "certificate" ? undefined : pinned,
    { command, launch, local, pause: async () => {} },
  );
  return { setup, command, launch, local, checkPublic, save };
}

describe("pinned gateway settings", () => {
  it("need only a listener: no address, certificate or key", () => {
    expect(validateGatewaySettings(pinned)).toEqual(pinned);
    expect(() =>
      validateGatewaySettings({ ...pinned, listen: "999.1.1.1:47321" }),
    ).toThrow("IP address");
    // Certificate mode still needs its files.
    expect(() =>
      validateGatewaySettings({
        ...pinned,
        mode: "certificate",
        url: "https://a.example",
      }),
    ).toThrow("certificate");
  });
  it("start the gateway without --tls-cert and --tls-key", () => {
    expect(gatewayLaunchArgs(pinned, "/log")).toEqual([
      "--log-file",
      "/log",
      "--listen",
      "0.0.0.0:47321",
    ]);
    expect(
      gatewayLaunchArgs(
        { ...pinned, mode: undefined, cert: "/c", key: "/k" },
        "/log",
      ),
    ).toEqual([
      "--log-file",
      "/log",
      "--listen",
      "0.0.0.0:47321",
      "--tls-cert",
      "/c",
      "--tls-key",
      "/k",
    ]);
  });
});

describe("the pinned gateway wizard", () => {
  it("starts a pinned gateway, checks it by the key --gateway-info reports, and shows its routes", async () => {
    const f = fixture({ running: false });
    const result = await f.setup.setup(pinned);
    expect(f.launch).toHaveBeenCalledOnce();
    expect(f.local).toHaveBeenCalledWith(
      pinned,
      expect.objectContaining({ pin: vectors.pin }),
    );
    expect(result).toMatchObject({
      tls: true,
      gatewayId: GATEWAY,
      pin: vectors.pin,
      routes: ["tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321"],
    });
    // No public URL, so no external check.
    expect(f.checkPublic).not.toHaveBeenCalled();
    expect(f.save).toHaveBeenCalledWith(pinned);
    expect(await f.setup.status()).toEqual({
      running: true,
      runningVersion: "0.7.0",
      listen: pinned.listen,
      restartRequired: false,
      pin: vectors.pin,
      routes: ["tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321"],
    });
  });
  it("asks for a restart to switch a certificate-mode gateway to pinned", async () => {
    const f = fixture({ running: "certificate" });
    expect(await f.setup.setup(pinned)).toMatchObject({
      restartRequired: true,
      tls: false,
    });
    expect(f.launch).not.toHaveBeenCalled();
  });
  it("makes a version 2 invitation from --pair-json and checks it against the gateway", async () => {
    const f = fixture();
    await f.setup.setup(pinned);
    const invitation = await f.setup.invite({ scopes: "operate" });
    expect(invitation).toMatchObject({
      version: 2,
      id: GATEWAY,
      pin: vectors.pin,
      code: vectors.code,
      link: vectors.invitation_link,
    });
    const args = f.command.mock.calls.find(([a]) => a[0] === "--pair")![0];
    expect(args).toEqual(["--pair", "--pair-only", "--json"]);
  });
  it.each([
    ["another key", { pin: "A".repeat(43) }],
    ["another gateway", { id: "11111111-1111-4111-8111-111111111111" }],
  ])("refuses an invitation for %s", async (_label, change) => {
    const f = fixture({
      invitation: {
        code: vectors.code,
        expires_at: 1763900000,
        gateway_id: GATEWAY,
        invitation: { ...JSON.parse(vectors.invitation_json), ...change },
        link: vectors.invitation_link,
      },
    });
    await f.setup.setup(pinned);
    await expect(f.setup.invite({ scopes: "operate" })).rejects.toThrow(
      "doesn't match",
    );
  });
  it("refuses a pinned gateway's output without an invitation", async () => {
    const f = fixture({
      invitation: { code: vectors.code, expires_at: 1, gateway_id: GATEWAY },
    });
    await f.setup.setup(pinned);
    await expect(f.setup.invite({ scopes: "operate" })).rejects.toThrow(
      "no pairing invitation",
    );
  });
  it("rolls a failed restart back to the pinned listener", async () => {
    const f = fixture();
    await f.setup.setup(pinned);
    f.local.mockRejectedValueOnce(
      new Error("PIN_MISMATCH: 127.0.0.1:47322: this is another device"),
    );
    const next = { ...pinned, listen: "0.0.0.0:47322" };
    await expect(f.setup.restart(next)).rejects.toThrow(
      "previous gateway configuration was restored",
    );
    expect(f.launch).toHaveBeenCalledTimes(2);
    expect(f.save).toHaveBeenLastCalledWith(pinned);
  });
});

describe("the local check in pinned mode", () => {
  async function gateway(identity: "hub" | "other", reported?: string) {
    const server: Server = createServer(
      { key: tls[identity].key, cert: tls[identity].cert },
      (_request, response) => {
        response.setHeader("Content-Type", "application/json");
        response.end(
          JSON.stringify({
            service: "twr-mobile",
            protocol: 2,
            gateway_id: GATEWAY,
            pin: reported ?? tls[identity].pin,
          }),
        );
      },
    );
    await new Promise<void>((resolve) =>
      server.listen(0, "127.0.0.1", resolve),
    );
    cleanup.push(() => new Promise((resolve) => server.close(() => resolve())));
    return {
      ...pinned,
      listen: `127.0.0.1:${(server.address() as AddressInfo).port}`,
    };
  }
  it("verifies the listener by the pin, not by OS trust", async () => {
    const settings = await gateway("hub");
    await expect(checkLocalPinned(settings, tls.hub.pin)).resolves.toEqual({
      url: "",
      gatewayId: GATEWAY,
      tls: true,
      pin: tls.hub.pin,
    });
  });
  it("refuses another key on the listener", async () => {
    const settings = await gateway("other");
    await expect(checkLocalPinned(settings, tls.hub.pin)).rejects.toThrow(
      "PIN_MISMATCH",
    );
  });
  it("refuses a listener whose /health reports another key", async () => {
    const settings = await gateway("hub", tls.other.pin);
    await expect(checkLocalPinned(settings, tls.hub.pin)).rejects.toThrow(
      "did not identify itself",
    );
  });
  it("refuses a gateway that reports no key", async () => {
    await expect(checkLocalPinned(pinned, undefined)).rejects.toThrow("no key");
  });
});
