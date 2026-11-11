import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createServer } from "node:https";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AddressInfo } from "node:net";
import type { TLSSocket } from "node:tls";
import { gatewayRequest } from "../desktop/gateway-network";
let directory = "";
let certificate = "";
let server: ReturnType<typeof createServer>;
let port = 0;
const seen: { sni: string; host: string }[] = [];
beforeAll(async () => {
  directory = mkdtempSync(join(tmpdir(), "twr-local-tls-"));
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
      join(directory, "key.pem"),
      "-out",
      join(directory, "cert.pem"),
      "-subj",
      "/CN=hub.example",
      "-addext",
      "subjectAltName=DNS:hub.example",
    ],
    { stdio: "ignore" },
  );
  certificate = readFileSync(join(directory, "cert.pem"), "utf8");
  server = createServer(
    { cert: certificate, key: readFileSync(join(directory, "key.pem")) },
    (request, response) => {
      seen.push({
        sni:
          (request.socket as TLSSocket & { servername?: string }).servername ||
          "",
        host: request.headers.host || "",
      });
      if (request.url === "/large") {
        response.end(JSON.stringify({ text: "x".repeat(20000) }));
        return;
      }
      if (request.url === "/redirect") {
        response.writeHead(302, { Location: "https://elsewhere.invalid" });
        response.end("{}");
        return;
      }
      response.end(
        JSON.stringify({
          service: "twr-mobile",
          protocol: 1,
          gateway_id: "local-test",
        }),
      );
    },
  );
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  port = (server.address() as AddressInfo).port;
});
afterAll(async () => {
  if (server)
    await new Promise<void>((resolve) => server.close(() => resolve()));
  if (directory) rmSync(directory, { recursive: true, force: true });
});
const local = () => ({
  connect: { host: "127.0.0.1", port },
  ca: [certificate],
  timeout: 3000,
});
describe("verified local TLS routing", () => {
  it("connects the local listener with the public hostname as SNI, Host and certificate identity", async () => {
    expect(
      await gatewayRequest("https://hub.example:443", "/health", local()),
    ).toMatchObject({ status: 200, data: { gateway_id: "local-test" } });
    expect(seen.at(-1)).toEqual({ sni: "hub.example", host: "hub.example" });
  });
  it("refuses untrusted certificates and hostname mismatch without a TLS bypass", async () => {
    await expect(
      gatewayRequest("https://hub.example", "/health", {
        connect: { host: "127.0.0.1", port },
        timeout: 3000,
      }),
    ).rejects.toThrow();
    await expect(
      gatewayRequest("https://wrong.example", "/health", local()),
    ).rejects.toThrow(/hostname|altnames|does not match/i);
  });
  it("keeps redirects local and bounds health and pairing response size", async () => {
    const before = seen.length;
    expect(
      (await gatewayRequest("https://hub.example", "/redirect", local()))
        .status,
    ).toBe(302);
    expect(seen.length).toBe(before + 1);
    await expect(
      gatewayRequest("https://hub.example", "/large", local()),
    ).rejects.toThrow("limit");
  });
});
