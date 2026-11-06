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
const available = Object.entries(devices.devices).flatMap(([runtime, list]) =>
  list.map((device) => ({ ...device, runtime })),
);

const targets = [
  available.find((d) => d.name.includes("iPhone") && !d.name.includes("Duo")),
  available.find((d) => d.name.includes("Duo")),
  available.find((d) => d.name.includes("iPad")),
].filter(Boolean);
assert(
  targets.some((d) => d.name.includes("Duo")) &&
    targets.some((d) => d.name.includes("iPhone") && !d.name.includes("Duo")),
  "Install iPhone and iOS 27.1 iPhone Duo Simulator runtimes",
);
const bin = resolve("../target/debug");
mkdirSync("test-results/ios", { recursive: true });
const kind = process.env.TWR_IOS_KIND;
const selected = targets.filter(
  (device) =>
    !kind ||
    (device.name.includes("Duo")
      ? "fold"
      : device.name.includes("iPad")
        ? "tablet"
        : "phone") === kind,
);
for (const [index, template] of selected.entries()) {
  const device = {
    ...template,
    udid: execFileSync(
      "xcrun",
      [
        "simctl",
        "create",
        `twr QA ${template.name}`,
        template.deviceTypeIdentifier,
        template.runtime,
      ],
      { encoding: "utf8" },
    ).trim(),
  };
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
    execFileSync("xcrun", ["simctl", "boot", device.udid], { stdio: "ignore" });
    execFileSync("xcrun", ["simctl", "bootstatus", device.udid, "-b"], {
      stdio: "inherit",
    });
    if (device.name.includes("Duo"))
      execFileSync(
        "npx",
        [
          "--yes",
          "agent-device@0.21.36",
          "fold",
          "closed",
          "--platform",
          "ios",
          "--udid",
          device.udid,
        ],
        { stdio: "inherit", timeout: 180000 },
      );
    let hubLog = "";
    hub = spawn(join(bin, "twr"), ["hub"], {
      env,
      stdio: ["ignore", "ignore", "pipe"],
    });
    hub.stderr.on("data", (chunk) => (hubLog += chunk));
    const hubDeadline = Date.now() + 120000;
    while (!existsSync(join(fixture, "hub.sock")) && Date.now() < hubDeadline) {
      assert(hub.exitCode === null, hubLog);
      await delay(200);
    }
    assert(
      existsSync(join(fixture, "hub.sock")),
      "Fixture hub must start: " + hubLog,
    );
    const info = JSON.parse(
      execFileSync(join(bin, "twr"), ["info", "--json"], {
        env,
        encoding: "utf8",
        timeout: 120000,
      }),
    );
    assert(info.pid, "Fixture hub must answer before starting the gateway");
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
    const started = Date.now();
    while (!stdout.includes("Pairing code") && Date.now() - started < 120000) {
      assert(gateway.exitCode === null, stderr);
      await delay(200);
    }
    const code = stdout.match(/Pairing code[^:]+: (\w+)/)?.[1];
    assert(
      code,
      "Gateway did not finish owner authentication and pairing initialization: " +
        stderr +
        "\nHub: " +
        hubLog,
    );
    console.log(`Gateway ready after ${Date.now() - started} ms`);
    server = createServer((req, res) => {
      res.setHeader("content-type", "application/json");
      if (req.url === "/fixture")
        res.end(
          JSON.stringify({ repo, code, foldable: device.name.includes("Duo") }),
        );
      else if (req.url === "/performance-script") {
        res.end(readFileSync(new URL("./ios-performance.js", import.meta.url)));
      } else if (req.url === "/performance" && req.method === "POST") {
        let body = "";
        req.on("data", (chunk) => {
          body += chunk;
          if (body.length > 1000000) req.destroy();
        });
        req.on("end", () => {
          const report = {
            ...JSON.parse(body),
            label: device.name.includes("Duo")
              ? "ios-fold"
              : device.name.includes("iPad")
                ? "ios-tablet"
                : "ios-phone",
          };
          writeFileSync(
            `test-results/performance-${report.label}.json`,
            JSON.stringify(report, null, 2) + "\n",
          );
          console.log("PERFORMANCE", JSON.stringify(report));
          res.end("ok");
        });
      } else if (req.url?.startsWith("/pose/") && device.name.includes("Duo")) {
        const pose = req.url.slice(6);
        if (!["closed", "open", "half-open"].includes(pose)) {
          res.statusCode = 400;
          res.end();
          return;
        }
        try {
          const result = execFileSync(
            "npx",
            [
              "--yes",
              "agent-device@0.21.36",
              "fold",
              pose,
              "--platform",
              "ios",
              "--udid",
              device.udid,
            ],
            { encoding: "utf8", timeout: 120000 },
          );
          console.log("FOLD", result);
          res.end("ok");
        } catch (error) {
          res.statusCode = 500;
          res.end(String(error));
        }
      } else if (req.url === "/verify-upload") {
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
    const result = `test-results/ios/${kind || "all"}-${index}-${device.name.replaceAll(" ", "-")}.xcresult`;
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
        "-test-timeouts-enabled",
        "YES",
        "-maximum-test-execution-time-allowance",
        "240",
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
        `test-results/ios/${kind || "all"}-${index}-screen.png`,
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
    execFileSync("xcrun", ["simctl", "delete", device.udid], {
      stdio: "ignore",
    });
    await delay(250);
    rmSync(fixture, { recursive: true, force: true });
  }
}
