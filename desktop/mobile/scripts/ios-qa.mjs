import { spawn, execFile, execFileSync } from "node:child_process";
import { promisify } from "node:util";
import { createInterface } from "node:readline";
import { createServer } from "node:http";
import { createServer as createProxy, connect as connectTCP } from "node:net";
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
const execFileAsync = promisify(execFile);
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
  const screenshot = `test-results/ios/${kind || "all"}-${index}-screen.png`;
  const foldHelper = join(fixture, "fold-helper");
  async function fold(pose) {
    const command = (args) =>
      execFileAsync(
        "xcrun",
        ["simctl", "spawn", device.udid, foldHelper, ...args],
        { timeout: 120000 },
      );
    await command(["set", pose]);
    const deadline = Date.now() + 60000;
    let previous;
    while (Date.now() < deadline) {
      const { stdout } = await command(["hinge", "0"]);
      const reading = JSON.parse(stdout.trim().split("\n").at(-1));
      const angle = reading.angleDegrees;
      assert(
        reading.angleValid && Number.isFinite(angle),
        "Simulator hinge reading must be valid",
      );
      const matches =
        pose === "closed"
          ? angle < 0.5
          : pose === "open"
            ? angle > 179.5
            : angle > 0.5 && angle < 179.5;
      if (
        matches &&
        previous !== undefined &&
        Math.abs(previous - angle) < 0.5
      ) {
        console.log("FOLD", JSON.stringify({ pose, angle }));
        return;
      }
      previous = angle;
      await delay(1000);
    }
    throw new Error(`Simulator did not settle in the ${pose} pose`);
  }
  rmSync(screenshot, { force: true });
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
    TWR_TRACE_STARTUP: join(fixture, "startup.trace"),
    SHELL: "/bin/sh",
  };
  let gateway, hub, server, proxy, owner;
  const proxySockets = new Set();
  let hubLog = "";
  try {
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
    const probeStarted = Date.now();
    const info = JSON.parse(
      execFileSync(join(bin, "twr"), ["info", "--json"], {
        env,
        encoding: "utf8",
        timeout: 120000,
      }),
    );
    assert(info.pid, "Fixture hub must answer before starting the gateway");
    console.log(
      `CLI hub ready after ${Date.now() - probeStarted} ms; identity ${info.identity}`,
    );
    if (device.name.includes("Duo")) {
      execFileSync(
        "xcrun",
        [
          "--sdk",
          "iphonesimulator",
          "clang",
          "-mios-simulator-version-min=15.0",
          "-fobjc-arc",
          "-Wall",
          "-Wextra",
          "-framework",
          "Foundation",
          "-framework",
          "IOKit",
          resolve("node_modules/agent-device/apple/fold-helper/Fold.m"),
          "-o",
          foldHelper,
        ],
        { stdio: "inherit", timeout: 120000 },
      );
    }
    gateway = spawn(
      join(bin, "twr-mobile-gateway"),
      ["--development", "--binary-dir", bin, "--listen", "127.0.0.1:47321"],
      { env, stdio: ["ignore", "pipe", "pipe"] },
    );
    let stderr = "";
    gateway.stdout.resume();
    gateway.stderr.on("data", (d) => (stderr += d));
    const started = Date.now();
    while (
      !stderr.includes("gateway listening on") &&
      Date.now() - started < 120000
    ) {
      assert(gateway.exitCode === null, stderr);
      await delay(200);
    }
    assert(
      stderr.includes("gateway listening on"),
      "Gateway did not finish owner authentication: " +
        stderr +
        "\nHub: " +
        hubLog,
    );
    console.log(`Gateway ready after ${Date.now() - started} ms`);
    // Authenticate the fixture owner before Simulator's cold boot overloads ps.
    // Keep this socket open and create a fresh five-minute code after boot.
    owner = connectTCP({ path: join(fixture, "hub.sock") });
    const replies = new Map();
    let nextID = 0;
    const lines = createInterface({ input: owner });
    lines.on("line", (line) => {
      const response = JSON.parse(line);
      replies.get(response.id)?.(response);
    });
    const ownerCall = (method, params) =>
      new Promise((resolve, reject) => {
        const id = ++nextID;
        const timer = setTimeout(() => {
          replies.delete(id);
          reject(new Error(`Fixture owner timed out: ${method}`));
        }, 120000);
        replies.set(id, (response) => {
          clearTimeout(timer);
          replies.delete(id);
          if (response.error) reject(new Error(JSON.stringify(response.error)));
          else resolve(response.result);
        });
        owner.write(JSON.stringify({ id, method, params }) + "\n");
      });
    const hello = await ownerCall("hello", {
      client: "ios-qa-owner",
      protocol: 1,
      user_token: readFileSync(token, "utf8").trim(),
    });
    assert(hello.identity.startsWith("user:"));
    execFileSync("xcrun", ["simctl", "boot", device.udid], { stdio: "ignore" });
    execFileSync("xcrun", ["simctl", "bootstatus", device.udid, "-b"], {
      stdio: "inherit",
    });
    if (device.name.includes("Duo")) {
      await fold("closed");
    }
    server = createServer(async (req, res) => {
      res.setHeader("content-type", "application/json");
      if (req.url === "/fixture") {
        try {
          // Xcode's test build/launch can exceed five minutes on a cold runner.
          // Issue the code only when the running app requests its fixture.
          const { code } = await ownerCall("device.pair_begin", {
            scopes: ["read", "operate", "admin", "terminal"],
          });
          res.end(
            JSON.stringify({
              repo,
              code,
              url: "http://127.0.0.1:47324",
              foldable: device.name.includes("Duo"),
            }),
          );
        } catch (error) {
          res.statusCode = 500;
          res.end(JSON.stringify({ error: String(error) }));
        }
      } else if (req.url === "/screenshot") {
        try {
          execFileSync(
            "xcrun",
            ["simctl", "io", device.udid, "screenshot", screenshot],
            {
              stdio: "inherit",
              timeout: 30000,
            },
          );
          res.end("ok");
        } catch (error) {
          res.statusCode = 500;
          res.end(String(error));
        }
      } else if (req.url === "/drop-socket") {
        for (const socket of proxySockets) socket.destroy();
        res.end("ok");
      } else if (req.url === "/performance-script") {
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
        void fold(pose)
          .then(() => res.end("ok"))
          .catch((error) => {
            res.statusCode = 500;
            res.end(String(error));
          });
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
    proxy = createProxy((socket) => {
      const upstream = connectTCP(47321, "127.0.0.1");
      for (const current of [socket, upstream]) {
        proxySockets.add(current);
        current.on("close", () => {
          proxySockets.delete(current);
          socket.destroy();
          upstream.destroy();
        });
        current.on("error", () => {
          socket.destroy();
          upstream.destroy();
        });
      }
      socket.pipe(upstream);
      upstream.pipe(socket);
    });
    await new Promise((resolve) => proxy.listen(47324, "127.0.0.1", resolve));
    const result = `test-results/ios/${kind || "all"}-${index}-${device.name.replaceAll(" ", "-")}.xcresult`;
    rmSync(result, { recursive: true, force: true });
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
        "-parallel-testing-enabled",
        "NO",
        "-test-timeouts-enabled",
        "YES",
        "-maximum-test-execution-time-allowance",
        "600",
        "CODE_SIGNING_ALLOWED=YES",
        "CODE_SIGN_IDENTITY=-",
      ],
      { stdio: "inherit" },
    );
    const exit = await new Promise((r) => test.on("exit", r));
    if (!existsSync(screenshot))
      execFileSync(
        "xcrun",
        ["simctl", "io", device.udid, "screenshot", screenshot],
        { stdio: "inherit" },
      );
    assert.equal(exit, 0, `${device.name} native integration tests must pass`);
    console.log(
      `PASS: ${device.name} native pairing, Keychain, streaming, answers, uploads, ordered PTY, reconnect and long-lived socket`,
    );
  } finally {
    for (const name of ["startup.trace", "data/hub.log"]) {
      const path = join(fixture, name);
      if (existsSync(path))
        writeFileSync(
          `test-results/ios/${kind || "all"}-${index}-${name.replaceAll("/", "-")}`,
          readFileSync(path),
        );
    }
    writeFileSync(
      `test-results/ios/${kind || "all"}-${index}-hub-stderr.log`,
      hubLog,
    );
    for (const socket of proxySockets) socket.destroy();
    await new Promise((r) => (proxy ? proxy.close(r) : r()));
    await new Promise((r) => (server ? server.close(r) : r()));
    owner?.destroy();
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
