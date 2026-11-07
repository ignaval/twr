// Real gateway lifecycle and read-only pairing against an isolated debug hub.
import { spawn, execFile, execFileSync } from "node:child_process";
import { promisify } from "node:util";
import {
  mkdtempSync,
  realpathSync,
  writeFileSync,
  readFileSync,
  renameSync,
  existsSync,
  rmSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { createServer } from "node:net";
import { randomUUID, createHash, X509Certificate } from "node:crypto";
import { WebSocket } from "ws";
import { request as httpsRequest } from "node:https";
import assert from "node:assert/strict";
const exec = promisify(execFile);
const bin = resolve("../target/debug");
const home = realpathSync(mkdtempSync("/tmp/twr-gateway-lifecycle-"));
const ownerToken = join(home, "owner-token");
writeFileSync(ownerToken, randomUUID(), { mode: 0o600 });
writeFileSync(
  join(home, "config.toml"),
  "[orchestrator]\nautostart = false\n[tui]\nsuggest = false\nsummaries = false\n",
  { mode: 0o600 },
);
const env = {
  ...process.env,
  TWR_HOME: home,
  TWR_TEST_USER_TOKEN_FILE: ownerToken,
};
const gatewayBinary = join(bin, "twr-mobile-gateway");
const command = async (args) =>
  JSON.parse(
    (await exec(gatewayBinary, args, { env, timeout: 15000, maxBuffer: 65536 }))
      .stdout,
  );
const delay = () => new Promise((resolve) => setTimeout(resolve, 50));
async function until(check) {
  for (let attempt = 0; attempt < 200; attempt++) {
    const value = await check().catch(() => undefined);
    if (value) return value;
    await delay();
  }
  throw new Error("Isolated gateway fixture did not become ready");
}
const probe = createServer();
await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
const port = probe.address().port;
await new Promise((resolve) => probe.close(resolve));
const hub = spawn(join(bin, "twr"), ["hub"], { env, stdio: "ignore" });
let gateway;
let socket;
let gatewayLog = "";
function start(tls = []) {
  const child = spawn(
    gatewayBinary,
    [
      ...(tls.length ? tls : ["--development"]),
      "--listen",
      `127.0.0.1:${port}`,
      "--binary-dir",
      bin,
    ],
    { env, stdio: ["ignore", "ignore", "pipe"] },
  );
  child.stderr.on("data", (chunk) => {
    gatewayLog = (gatewayLog + chunk).slice(-4096);
  });
  return child;
}
try {
  await until(async () => existsSync(join(home, "hub.sock")));
  gateway = start();
  const first = await until(async () => {
    const info = await command(["--gateway-info"]);
    return info.running && info;
  });
  assert.equal(first.pid, gateway.pid);
  const unused = await command([
    "--pair",
    "--pair-only",
    "--json",
    "--read-only",
  ]);
  const cancelled = await new Promise((resolve, reject) => {
    const child = execFile(
      gatewayBinary,
      ["--cancel-invitation"],
      { env, timeout: 15000 },
      (error, stdout) => (error ? reject(error) : resolve(JSON.parse(stdout))),
    );
    child.stdin.end(JSON.stringify({ code: unused.code }));
  });
  assert.equal(cancelled.cancelled, true);
  const refused = await fetch(`http://127.0.0.1:${port}/v1/pair`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code: unused.code, name: "Cancelled invitation" }),
  });
  assert.equal(
    refused.status,
    401,
    "Closing an unused invitation must revoke its pairing code",
  );
  const invitation = await command([
    "--pair",
    "--pair-only",
    "--json",
    "--read-only",
  ]);
  assert.deepEqual(invitation.scopes, ["read"]);
  const base = `http://127.0.0.1:${port}`;
  const pair = () =>
    fetch(`${base}/v1/pair`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        code: invitation.code,
        name: "Lifecycle read-only QA",
      }),
    });
  const paired = await pair();
  assert.equal(paired.status, 200);
  const credentials = await paired.json();
  assert.deepEqual(credentials.scopes, ["read"]);
  assert.equal(
    (await pair()).status,
    401,
    "The invitation must be consumed once",
  );
  const pending = new Map();
  socket = new WebSocket(base.replace("http:", "ws:") + "/v1/socket", {
    headers: { Authorization: `Bearer ${credentials.token}` },
  });
  await new Promise((resolve, reject) => {
    socket.on("error", reject);
    socket.on("message", (bytes) => {
      const frame = JSON.parse(bytes.toString());
      if (frame.type === "authenticated") resolve();
      const listener = pending.get(frame.id);
      if (listener) {
        pending.delete(frame.id);
        listener(frame);
      }
    });
  });
  const request = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = randomUUID();
      const timer = setTimeout(
        () => reject(new Error("Gateway fixture request timed out")),
        10000,
      );
      pending.set(id, (frame) => {
        clearTimeout(timer);
        resolve(frame);
      });
      socket.send(JSON.stringify({ id, method, params, workspace: "default" }));
    });
  const connected = await request("connect", { workspace: "default" });
  assert(!connected.error, connected.error);
  const read = await request("rpc", { method: "hub.info", params: {} });
  assert.equal(read.result.workspace, "default");
  const denied = await request("rpc", {
    method: "thread.send",
    params: { thread: "missing", prompt: "must not execute" },
  });
  assert.match(denied.error, /permission|scope|allowed/i);
  await command(["--stop-gateway", "--expected-instance", first.instance]);
  await until(async () => gateway.exitCode !== null);
  assert.equal(
    hub.exitCode,
    null,
    "Gateway restart must leave the independent hub running",
  );
  gateway = start();
  const second = await until(async () => {
    const info = await command(["--gateway-info"]);
    return info.running && info;
  });
  assert.notEqual(second.instance, first.instance);
  assert.equal(second.pid, gateway.pid);
  await assert.rejects(
    command(["--stop-gateway", "--expected-instance", first.instance]),
  );
  assert.equal(
    (await command(["--gateway-info"])).instance,
    second.instance,
    "A stale restart must not stop the replacement process",
  );
  await command(["--stop-gateway", "--expected-instance", second.instance]);
  assert.equal(hub.exitCode, null);
  await until(async () => gateway.exitCode !== null);
  const certificatePath = join(home, "gateway.pem");
  const keyPath = join(home, "gateway.key");
  function issue(suffix) {
    const cert = join(home, `${suffix}.pem`),
      key = join(home, `${suffix}.key`);
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
        key,
        "-out",
        cert,
        "-subj",
        "/CN=hub.example",
        "-addext",
        "subjectAltName=DNS:hub.example",
      ],
      { stdio: "ignore" },
    );
    const content = readFileSync(cert, "utf8");
    renameSync(cert, certificatePath);
    renameSync(key, keyPath);
    return content;
  }
  const firstCertificate = issue("initial");
  let trusted = [firstCertificate];
  const tlsHealth = () =>
    new Promise((resolve, reject) => {
      const request = httpsRequest(
        {
          hostname: "127.0.0.1",
          port,
          path: "/health",
          servername: "hub.example",
          ca: [...trusted],
          agent: false,
        },
        (response) => {
          const peer = response.socket.getPeerCertificate().fingerprint256;
          response.resume();
          response.on("end", () => resolve(peer));
        },
      );
      request.on("error", reject);
      request.setTimeout(2000, () =>
        request.destroy(new Error("TLS fixture timed out")),
      );
      request.end();
    });
  gateway = start(["--tls-cert", certificatePath, "--tls-key", keyPath]);
  const tlsInfo = await until(async () => {
    const info = await command(["--gateway-info"]);
    return info.running && info;
  });
  assert.equal(
    await until(tlsHealth),
    new X509Certificate(firstCertificate).fingerprint256,
  );
  const renewed = issue("renewed");
  trusted = [renewed];
  let lastTLS;
  try {
    await until(async () => {
      try {
        const fingerprint = await tlsHealth();
        lastTLS = { fingerprint };
        return fingerprint === new X509Certificate(renewed).fingerprint256;
      } catch (error) {
        lastTLS = { error: error.message };
        throw error;
      }
    });
  } catch (error) {
    console.error("TLS renewal diagnostics", {
      lastTLS,
      expected: new X509Certificate(renewed).fingerprint256,
      metadata: await command(["--gateway-info"]),
      fileDigest: createHash("sha256")
        .update(readFileSync(certificatePath))
        .digest("hex"),
    });
    throw error;
  }
  const digest = createHash("sha256").update(renewed).digest("hex");
  await until(
    async () =>
      (await command(["--gateway-info"])).certificate_sha256 === digest,
  );
  const failures = (gatewayLog.match(/could not be loaded/g) || []).length;
  writeFileSync(certificatePath, "invalid renewal");
  gateway.kill("SIGHUP");
  await until(
    async () =>
      (gatewayLog.match(/could not be loaded/g) || []).length > failures,
  );
  assert.equal(
    await tlsHealth(),
    new X509Certificate(renewed).fingerprint256,
    "An invalid renewal must retain the last valid TLS context",
  );
  assert.equal((await command(["--gateway-info"])).certificate_sha256, digest);
  assert.equal((await command(["--gateway-info"])).renewal_state, "failed");
  await command(["--stop-gateway", "--expected-instance", tlsInfo.instance]);
  assert.equal(hub.exitCode, null);
  console.log(
    "Gateway lifecycle QA passed: verified process metadata, one-use read-only pairing, scoped RPC denial, gateway-only restart, stale-instance fencing and valid/invalid TLS renewal.",
  );
} catch (error) {
  if (gatewayLog) console.error(gatewayLog);
  console.error("Gateway fixture exit", gateway?.exitCode, gateway?.signalCode);
  throw error;
} finally {
  socket?.terminate();
  gateway?.kill();
  hub.kill();
  await delay();
  rmSync(home, { recursive: true, force: true });
}
