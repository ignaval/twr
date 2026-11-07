import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { validateGatewaySettings } from "../desktop/gateway-setup";
let directory = "";
beforeAll(() => {
  directory = mkdtempSync(join(tmpdir(), "twr-gateway-tls-"));
  for (const name of ["host", "other"])
    execFileSync(
      "openssl",
      [
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-keyout",
        join(directory, `${name}.key`),
        "-out",
        join(directory, `${name}.pem`),
        "-subj",
        `/CN=${name}.example`,
        "-addext",
        `subjectAltName=DNS:${name}.example`,
      ],
      { stdio: "ignore" },
    );
});
afterAll(() => {
  if (directory) rmSync(directory, { recursive: true, force: true });
});
const settings = () => ({
  url: "https://host.example:47321",
  listen: "0.0.0.0:47321",
  cert: join(directory, "host.pem"),
  key: join(directory, "host.key"),
  scopes: "read" as const,
});
describe("gateway TLS preflight", () => {
  it("accepts a current certificate with matching hostname and private key", () => {
    expect(validateGatewaySettings(settings())).toEqual(settings());
  });
  it("rejects wrong hostnames, mismatched keys and invalid listening addresses", () => {
    expect(() =>
      validateGatewaySettings({
        ...settings(),
        url: "https://wrong.example:47321",
      }),
    ).toThrow("hostname");
    expect(() =>
      validateGatewaySettings({
        ...settings(),
        key: join(directory, "other.key"),
      }),
    ).toThrow("match");
    expect(() =>
      validateGatewaySettings({ ...settings(), listen: "999.1.1.1:47321" }),
    ).toThrow("IP address");
    expect(() =>
      validateGatewaySettings({ ...settings(), listen: "0.0.0.0:65536" }),
    ).toThrow("port");
    expect(() =>
      validateGatewaySettings({ ...settings(), key: "relative.key" }),
    ).toThrow("absolute");
  });
});

import { vi } from "vitest";
import { GatewaySetup, certificateDigest } from "../desktop/gateway-setup";
import { realpathSync } from "node:fs";
function setupFixture(
  options: {
    running?: boolean;
    mismatched?: boolean;
    publicFails?: boolean;
    childExit?: number;
  } = {},
) {
  const configured = settings();
  const runningInfo = () => ({
    running: true,
    gateway_id: "own-gateway",
    pid: 123,
    instance: "11111111-1111-4111-8111-111111111111",
    version: "0.3.0",
    executable: realpathSync(process.execPath),
    tls_cert_source: configured.cert,
    tls_key_source: configured.key,
    renewal_state: "current",
    listen: configured.listen,
    tls_cert: realpathSync(configured.cert),
    tls_key: realpathSync(configured.key),
    certificate_sha256: certificateDigest(configured.cert),
    started_at: Date.now() / 1000,
  });
  let info: any =
    options.running === false
      ? { running: false, gateway_id: "own-gateway" }
      : runningInfo();
  const command = vi.fn(async (args: string[], input?: string) => {
    if (args[0] === "--cancel-invitation") {
      expect(JSON.parse(input || "{}").code).toBe("ABCDEF2345");
      return { cancelled: true };
    }
    if (args[0] === "--gateway-info") return info;
    if (args[0] === "--stop-gateway") {
      expect(args).toEqual([
        "--stop-gateway",
        "--expected-instance",
        info.instance,
      ]);
      info = { running: false, gateway_id: "own-gateway" };
      return { stopped: true };
    }
    return {
      code: "ABCDEF2345",
      expires_at: Math.floor(Date.now() / 1000) + 300,
      gateway_id: "own-gateway",
    };
  });
  const launch = vi.fn(async () => {
    if (!options.childExit) info = runningInfo();
    return { exitCode: () => options.childExit ?? null, signal: () => null };
  });
  const local = vi.fn(async () => ({
    url: configured.url,
    gatewayId: options.mismatched ? "foreign-gateway" : "own-gateway",
    tls: true,
  }));
  const checkPublic = vi.fn(async () => {
    if (options.publicFails) throw new Error("NAT loopback unavailable");
    return { url: configured.url, gatewayId: "own-gateway", tls: true };
  });
  const save = vi.fn();
  const logPath = join(directory, "gateway.log");
  const setup = new GatewaySetup(
    process.execPath,
    () => ({}),
    logPath,
    checkPublic,
    save,
    configured,
    { command, launch, local, pause: async () => {} },
  );
  return {
    setup,
    configured,
    command,
    launch,
    local,
    checkPublic,
    save,
    logPath,
    replaceInfo: (value: any) => {
      info = value;
    },
    runningInfo,
  };
}
describe("guided gateway lifecycle", () => {
  it("stops only the owned gateway and permits starting with replacement settings afterwards", async () => {
    const f = setupFixture();
    await f.setup.stop();
    expect(await f.setup.status()).toMatchObject({ running: false });
    expect(f.command.mock.calls.some(([args]) => args.includes("hub"))).toBe(
      false,
    );
    expect(await f.setup.setup(f.configured)).toMatchObject({ tls: true });
    expect(f.launch).toHaveBeenCalledOnce();
  });
  it("probes running and stopped status without launching or checking the public route", async () => {
    const f = setupFixture();
    expect(await f.setup.status()).toMatchObject({
      running: true,
      runningVersion: "0.3.0",
      restartRequired: false,
    });
    f.replaceInfo({ running: false, gateway_id: "own-gateway" });
    expect(await f.setup.status()).toEqual({ running: false });
    expect(f.launch).not.toHaveBeenCalled();
    expect(f.local).not.toHaveBeenCalled();
  });
  it("treats digest-only rotation as renewal, keeps invitations available, and exposes failed renewal diagnostics", async () => {
    const f = setupFixture();
    f.replaceInfo({
      ...f.runningInfo(),
      certificate_sha256: "previous",
      renewal_state: "failed",
      renewal_error: "Invalid renewal file",
    });
    writeFileSync(f.logPath, "Gateway TLS renewal failed\n");
    expect(await f.setup.status()).toMatchObject({
      restartRequired: false,
      renewalState: "failed",
      logTail: expect.stringContaining("failed"),
    });
    expect(await f.setup.setup(f.configured)).toMatchObject({
      tls: true,
      advisory: expect.stringContaining("renewal failed"),
    });
    expect(f.launch).not.toHaveBeenCalled();
    await expect(f.setup.invite({ scopes: "read" })).resolves.toHaveProperty(
      "code",
    );
  });
  it("cancels invitations on replacement and close without putting codes in argv", async () => {
    const f = setupFixture();
    await f.setup.invite({ scopes: "read" });
    await f.setup.invite({ scopes: "terminal" });
    await f.setup.cancelInvitation();
    expect(
      f.command.mock.calls.filter(
        ([args]) => args[0] === "--cancel-invitation",
      ),
    ).toEqual([
      [["--cancel-invitation"], JSON.stringify({ code: "ABCDEF2345" })],
      [["--cancel-invitation"], JSON.stringify({ code: "ABCDEF2345" })],
    ]);
    expect(
      f.command.mock.calls.every(([args]) => !args.includes("ABCDEF2345")),
    ).toBe(true);
  });
  it("stops an unverified new child and restores the previous working configuration when restart fails", async () => {
    const f = setupFixture();
    const stop = vi.fn(async () =>
      f.replaceInfo({ running: false, gateway_id: "own-gateway" }),
    );
    f.launch.mockImplementationOnce(async () => {
      f.replaceInfo({ ...f.runningInfo(), listen: "0.0.0.0:12346" });
      return { exitCode: () => null, signal: () => null, stop };
    });
    f.local.mockRejectedValueOnce(new Error("verification failed"));
    // A trusted verification failure is terminal; avoid retrying a broken startup.
    f.local.mockRejectedValueOnce(
      Object.assign(new Error("verification failed"), {
        code: "ERR_TLS_CERT_ALTNAME_INVALID",
      }),
    );
    await expect(
      f.setup.restart({ ...f.configured, listen: "0.0.0.0:12346" }),
    ).rejects.toThrow("previous gateway configuration was restored");
    expect(stop).toHaveBeenCalledOnce();
    expect(f.launch).toHaveBeenCalledTimes(2);
    expect(f.setup.saved()).toEqual(f.configured);
  });

  it("reuses a verified running gateway and mints scoped invitations without restarting it", async () => {
    const f = setupFixture();
    expect(await f.setup.setup(f.configured)).toMatchObject({
      tls: true,
      externalReachable: true,
    });
    expect(f.launch).not.toHaveBeenCalled();
    for (const [scopes, flags] of [
      ["read", ["--read-only"]],
      ["operate", []],
      ["terminal", ["--allow-terminal"]],
      ["admin", ["--allow-terminal", "--allow-admin"]],
    ] as const) {
      const invitation = await f.setup.invite({ scopes });
      expect(invitation).toMatchObject({
        url: f.configured.url,
        code: "ABCDEF2345",
      });
      expect(invitation.expiresAt).toBeGreaterThan(Date.now());
      expect(f.command).toHaveBeenLastCalledWith([
        "--pair",
        "--pair-only",
        "--json",
        ...flags,
      ]);
    }
    expect(f.save).toHaveBeenLastCalledWith({
      ...f.configured,
      scopes: "admin",
    });
  });
  it("starts a missing gateway, requires local TLS and treats NAT-loopback failure as advisory", async () => {
    const f = setupFixture({ running: false, publicFails: true });
    expect(await f.setup.setup(f.configured)).toMatchObject({
      tls: true,
      externalReachable: false,
      advisory: expect.stringContaining("NAT"),
    });
    expect(f.launch).toHaveBeenCalledOnce();
    expect(f.local).toHaveBeenCalled();
    await expect(f.setup.invite({ scopes: "read" })).resolves.toHaveProperty(
      "code",
    );
  });
  it("does not create a code when the local TLS listener has another gateway identity", async () => {
    const f = setupFixture({ mismatched: true });
    await expect(f.setup.setup(f.configured)).rejects.toThrow(
      "another gateway",
    );
    await expect(f.setup.invite({ scopes: "read" })).rejects.toThrow(
      "another gateway",
    );
    expect(
      f.command.mock.calls.every(([args]) => args[0] === "--gateway-info"),
    ).toBe(true);
    expect(f.save).not.toHaveBeenCalled();
  });
  it("requires an explicit restart for another engine and stops only its verified instance", async () => {
    const f = setupFixture();
    f.replaceInfo({
      ...f.runningInfo(),
      listen: "127.0.0.1:12345",
    });
    expect(await f.setup.setup(f.configured)).toMatchObject({
      restartRequired: true,
      tls: false,
    });
    expect(f.launch).not.toHaveBeenCalled();
    expect(
      f.command.mock.calls.some(([args]) => args[0] === "--stop-gateway"),
    ).toBe(false);
    expect(await f.setup.restart(f.configured)).toMatchObject({ tls: true });
    expect(f.launch).toHaveBeenCalledOnce();
    expect(f.command.mock.calls.some(([args]) => args.includes("hub"))).toBe(
      false,
    );
  });
  it("reports an exited child immediately with a bounded log tail", async () => {
    const f = setupFixture({ running: false, childExit: 1 });
    writeFileSync(
      f.logPath,
      "old log\n".repeat(2000) + "Address already in use\n",
    );
    let failure: unknown;
    try {
      await f.setup.setup(f.configured);
    } catch (error) {
      failure = error;
    }
    expect(String(failure)).toContain("status 1");
    expect(String(failure)).toContain("Address already in use");
    expect(String(failure).length).toBeLessThan(4500);
    expect(f.local).not.toHaveBeenCalled();
  });
});
