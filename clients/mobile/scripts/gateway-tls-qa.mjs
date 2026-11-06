import { spawn, execFileSync } from "node:child_process";
import {
  mkdtempSync,
  writeFileSync,
  readFileSync,
  existsSync,
  rmSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { randomUUID, randomBytes } from "node:crypto";
import https from "node:https";
import assert from "node:assert/strict";
const home = mkdtempSync(join(tmpdir(), "twr-gateway-tls-"));
const bin = resolve("../target/debug");
writeFileSync(
  join(home, "config.toml"),
  "[orchestrator]\nautostart=false\n[tui]\nsuggest=false\nsummaries=false\n",
  { mode: 0o600 },
);
const token = join(home, "local-test-token");
writeFileSync(token, randomUUID(), { mode: 0o600 });
const env = {
  ...process.env,
  HOME: home,
  TWR_HOME: home,
  TWR_TEST_USER_TOKEN_FILE: token,
};
const cert = join(home, "cert.pem"),
  key = join(home, "key.pem");
execFileSync(
  "openssl",
  [
    "req",
    "-x509",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-keyout",
    key,
    "-out",
    cert,
    "-days",
    "1",
    "-subj",
    "/CN=localhost",
    "-addext",
    "subjectAltName=IP:127.0.0.1,DNS:localhost",
  ],
  { stdio: "ignore" },
);
const ca = readFileSync(cert);
let hub,
  gateway,
  out = "",
  err = "";
const request = (path, { body, headers = {}, upgrade = false } = {}) =>
  new Promise((resolve, reject) => {
    const req = https.request(
      {
        hostname: "127.0.0.1",
        port: 47401,
        path,
        ca,
        method: body ? "POST" : "GET",
        headers: {
          ...headers,
          ...(body ? { "Content-Type": "application/json" } : {}),
        },
        timeout: 5000,
      },
      (res) => {
        let text = "";
        res.on("data", (d) => (text += d));
        res.on("end", () => resolve({ status: res.statusCode, text }));
      },
    );
    req.on("upgrade", (res, socket) => {
      socket.destroy();
      resolve({ status: res.statusCode });
    });
    req.on("timeout", () => req.destroy(new Error("TLS request timeout")));
    req.on("error", reject);
    if (body) req.write(JSON.stringify(body));
    req.end();
  });
try {
  hub = spawn(join(bin, "twr"), ["hub"], { env, stdio: "ignore" });
  for (let i = 0; i < 100 && !existsSync(join(home, "hub.sock")); i++)
    await new Promise((r) => setTimeout(r, 100));
  gateway = spawn(
    join(bin, "twr-mobile-gateway"),
    [
      "--listen",
      "127.0.0.1:47401",
      "--tls-cert",
      cert,
      "--tls-key",
      key,
      "--pair",
      "--binary-dir",
      bin,
    ],
    { env, stdio: ["ignore", "pipe", "pipe"] },
  );
  gateway.stdout.on("data", (d) => (out += d));
  gateway.stderr.on("data", (d) => (err += d));
  for (let i = 0; i < 100 && !out.includes("Pairing code"); i++) {
    assert.equal(gateway.exitCode, null, err);
    await new Promise((r) => setTimeout(r, 100));
  }
  const code = out.match(/Pairing code[^:]+: (\w+)/)?.[1];
  assert(code, err);
  await new Promise((r) => setTimeout(r, 200));
  assert.equal((await request("/health")).status, 200);
  const headers = {
    Connection: "Upgrade",
    Upgrade: "websocket",
    "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
    "Sec-WebSocket-Version": "13",
  };
  assert.equal((await request("/v1/socket", { headers })).status, 401);
  const pair = await request("/v1/pair", { body: { code, name: "TLS QA" } });
  assert.equal(pair.status, 200, pair.text);
  const device = JSON.parse(pair.text);
  assert.equal(
    (await request("/v1/pair", { body: { code, name: "Replay" } })).status,
    401,
  );
  assert.equal(
    (
      await request("/v1/socket", {
        headers: { ...headers, Authorization: `Bearer ${device.token}` },
        upgrade: true,
      })
    ).status,
    101,
  );
  execFileSync(
    join(bin, "twr-mobile-gateway"),
    ["--revoke", device.id, "--binary-dir", bin],
    { env, stdio: "ignore" },
  );
  assert.equal(
    (
      await request("/v1/socket", {
        headers: { ...headers, Authorization: `Bearer ${device.token}` },
      })
    ).status,
    401,
  );
  console.log(
    "PASS: real TLS listener, trusted certificate, auth before WebSocket upgrade, one-use pairing and immediate revocation",
  );
} finally {
  gateway?.kill();
  hub?.kill();
  await new Promise((r) => setTimeout(r, 500));
  rmSync(home, {
    recursive: true,
    force: true,
    maxRetries: 10,
    retryDelay: 100,
  });
}
