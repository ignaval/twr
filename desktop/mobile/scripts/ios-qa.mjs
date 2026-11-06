import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  readdirSync,
  existsSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import assert from "node:assert/strict";
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const devices = JSON.parse(
  execFileSync("xcrun", ["simctl", "list", "devices", "available", "--json"], {
    encoding: "utf8",
  }),
);
const available = Object.values(devices.devices).flat();
const targets = [
  available.find((d) => d.name.includes("iPhone") && !d.name.includes("Duo")),
  available.find((d) => d.name.includes("Duo")),
].filter(Boolean);
assert(
  targets.some((d) => d.name.includes("Duo")),
  "Install the iOS 27.1 iPhone Duo Simulator runtime",
);
const bin = resolve("../target/debug");
mkdirSync("test-results/ios", { recursive: true });
for (const [index, device] of targets.entries()) {
  const fixture = mkdtempSync(join(tmpdir(), "twr-ios-"));
  const repo = join(fixture, "repo");
  mkdirSync(repo);
  execFileSync("git", ["init", "-b", "main", repo], { stdio: "ignore" });
  execFileSync(
    "git",
    [
      "-C",
      repo,
      "-c",
      "user.name=QA",
      "-c",
      "user.email=qa@example.invalid",
      "commit",
      "--allow-empty",
      "-m",
      "Initial",
    ],
    { stdio: "ignore" },
  );
  const token = join(fixture, "test-user-token");
  writeFileSync(token, randomUUID(), { mode: 0o600 });
  writeFileSync(
    join(fixture, "config.toml"),
    `default_runtime = "fake"\n[projects.demo]\nrepo = ${JSON.stringify(repo)}\n[orchestrator]\nruntime = "fake"\nautostart = false\n[tui]\nsuggest = false\nsummaries = false\n`,
    { mode: 0o600 },
  );
  const env = {
    ...process.env,
    TWR_HOME: fixture,
    HOME: fixture,
    TWR_TEST_USER_TOKEN_FILE: token,
    TWR_ENABLE_FAKE: "1",
    SHELL: "/bin/sh",
  };
  let gateway, hub, server;
  try {
    hub = spawn(join(bin, "twr"), ["hub"], { env, stdio: "ignore" });
    for (let i = 0; i < 150 && !existsSync(join(fixture, "hub.sock")); i++)
      await delay(100);
    assert(existsSync(join(fixture, "hub.sock")), "Fixture hub must start");
    gateway = spawn(
      join(bin, "twr-mobile-gateway"),
      [
        "--development",
        "--pair",
        "--allow-admin",
        "--allow-terminal",
        "--binary-dir",
        bin,
        "--listen",
        "127.0.0.1:47321",
      ],
      { env, stdio: ["ignore", "pipe", "pipe"] },
    );
    let stdout = "",
      stderr = "";
    gateway.stdout.on("data", (d) => (stdout += d));
    gateway.stderr.on("data", (d) => (stderr += d));
    for (let i = 0; i < 150 && !stdout.includes("Pairing code"); i++) {
      assert(gateway.exitCode === null, stderr);
      await delay(100);
    }
    const code = stdout.match(/Pairing code[^:]+: (\w+)/)?.[1];
    assert(code, stderr);
    server = createServer((req, res) => {
      res.setHeader("content-type", "application/json");
      if (req.url === "/fixture") res.end(JSON.stringify({ repo, code }));
      else if (req.url === "/verify-upload") {
        const files = readdirSync(fixture, {
          recursive: true,
          withFileTypes: true,
        });
        const file = files.find((f) => f.isFile() && f.name === "ios-qa.txt");
        if (!file) {
          res.statusCode = 404;
          res.end("Upload not found");
        } else res.end(readFileSync(join(file.parentPath, file.name)));
      } else {
        res.statusCode = 404;
        res.end();
      }
    });
    await new Promise((r) => server.listen(47323, "127.0.0.1", r));
    execFileSync("xcrun", ["simctl", "boot", device.udid], { stdio: "ignore" });
    execFileSync("xcrun", ["simctl", "bootstatus", device.udid, "-b"], {
      stdio: "inherit",
    });
    const result = `test-results/ios/${index}-${device.name.replaceAll(" ", "-")}.xcresult`;
    const test = spawn(
      "xcodebuild",
      [
        "test",
        "-project",
        "mobile/ios/App/App.xcodeproj",
        "-scheme",
        "App",
        "-configuration",
        "Debug",
        "-destination",
        `platform=iOS Simulator,id=${device.udid}`,
        "-derivedDataPath",
        "mobile/ios/App/DerivedData",
        "-resultBundlePath",
        result,
        "CODE_SIGNING_ALLOWED=NO",
      ],
      { stdio: "inherit" },
    );
    const exit = await new Promise((r) => test.on("exit", r));
    execFileSync(
      "xcrun",
      [
        "simctl",
        "io",
        device.udid,
        "screenshot",
        `test-results/ios/${index}-screen.png`,
      ],
      { stdio: "inherit" },
    );
    assert.equal(exit, 0, `${device.name} native integration tests must pass`);
    console.log(
      `PASS: ${device.name} native pairing, Keychain, streaming, answers, uploads, ordered PTY, reconnect and long-lived socket`,
    );
  } finally {
    await new Promise((r) => (server ? server.close(r) : r()));
    gateway?.kill("SIGTERM");
    hub?.kill("SIGTERM");
    try {
      execFileSync("xcrun", ["simctl", "shutdown", device.udid], {
        stdio: "ignore",
      });
    } catch {}
    await delay(250);
    rmSync(fixture, { recursive: true, force: true });
  }
}
