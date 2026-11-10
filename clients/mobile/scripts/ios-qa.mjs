import { clientProtocol } from "../../scripts/client-protocol.mjs";
import { stopFixture } from "../../scripts/fixture-cleanup.mjs";
import { spawn, execFile, execFileSync } from "node:child_process";
import { promisify } from "node:util";
import { createInterface } from "node:readline";
import { createServer } from "node:http";
import { createServer as createProxy, connect as connectTCP } from "node:net";
import {
  mkdtempSync,
  realpathSync,
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
import { qaSource } from "../../scripts/qa-source.mjs";
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const kind = process.env.TWR_IOS_KIND?.trim() || "all";
const defaultRuntimes = { phone: "27.0", fold: "27.1", tablet: "27.0" };
assert(
  kind === "all" || Object.hasOwn(defaultRuntimes, kind),
  `Unsupported TWR_IOS_KIND=${kind}; use phone, fold, tablet, or all`,
);
const requestedKinds = kind === "all" ? Object.keys(defaultRuntimes) : [kind];
const runtimeOverride = process.env.TWR_IOS_RUNTIME?.trim();
const { runtimes } = JSON.parse(
  execFileSync("xcrun", ["simctl", "list", "runtimes", "--json"], {
    encoding: "utf8",
  }),
);
const iosRuntimes = runtimes.filter((runtime) =>
  runtime.identifier.startsWith("com.apple.CoreSimulator.SimRuntime.iOS-"),
);
const runtimeDescription = (runtime) => ({
  name: runtime.name,
  version: runtime.version,
  build: runtime.buildversion,
  identifier: runtime.identifier,
  available: runtime.isAvailable === true,
});
console.log(
  "IOS_SIMULATOR_RUNTIMES",
  JSON.stringify(iosRuntimes.map(runtimeDescription)),
);
const devices = JSON.parse(
  execFileSync("xcrun", ["simctl", "list", "devices", "available", "--json"], {
    encoding: "utf8",
  }),
);
const available = Object.entries(devices.devices).flatMap(([runtime, list]) =>
  list.map((device) => ({ ...device, runtime })),
);

const deviceKind = (device) =>
  device.name.includes("Duo")
    ? "fold"
    : device.name.includes("iPad")
      ? "tablet"
      : device.name.includes("iPhone")
        ? "phone"
        : undefined;
const usableRuntimes = iosRuntimes.filter(
  (runtime) => runtime.isAvailable === true,
);
const hasDevice = (runtime, family) =>
  available.some(
    (device) =>
      device.runtime === runtime.identifier && deviceKind(device) === family,
  );
// The general runtime (`iOS-27-2`), not a device-specific one (`iOS-27-2-iPhone19-4`).
const isPlain = (runtime) =>
  /^com\.apple\.CoreSimulator\.SimRuntime\.iOS-\d+-\d+$/.test(
    runtime.identifier,
  );
const newestFirst = (a, b) => {
  const [x, y] = [a, b].map((r) => r.version.split(".").map(Number));
  for (let i = 0; i < Math.max(x.length, y.length); i++)
    if ((y[i] || 0) !== (x[i] || 0)) return (y[i] || 0) - (x[i] || 0);
  return 0;
};
/**
 * The one runtime among `candidates` for `family`: one with a device of that kind,
 * preferring the plain runtime for phone and tablet when several share a version
 * (Xcode 27.2 installs `iOS-27-2` and the Duo's `iOS-27-2-iPhone19-4`, both 27.2).
 */
const runtimeFor = (family, candidates) => {
  const withDevice = candidates.filter((runtime) => hasDevice(runtime, family));
  const plain = withDevice.filter(isPlain);
  return family !== "fold" && plain.length >= 1 && withDevice.length > 1
    ? plain
    : withDevice;
};
// Runtime/device-type compatibility is not interchangeable: the fold runtime
// can reject phone and tablet types. Validate every requested family up front.
const selected = requestedKinds.map((family) => {
  let runtime;
  if (runtimeOverride?.startsWith("com.apple.CoreSimulator.SimRuntime.")) {
    // An explicit identifier is taken as it is, or the run fails.
    runtime = usableRuntimes.find((r) => r.identifier === runtimeOverride);
    assert(
      runtime,
      `TWR_IOS_RUNTIME '${runtimeOverride}' is not an installed, available runtime. Install it in Xcode Settings > Components. Inspect: xcrun simctl list runtimes --json. No alternate runtime will be selected.`,
    );
  } else {
    const version = runtimeOverride || defaultRuntimes[family];
    const matching = runtimeFor(
      family,
      usableRuntimes.filter((r) => r.version === version),
    );
    if (runtimeOverride)
      assert.equal(
        matching.length,
        1,
        `TWR_IOS_RUNTIME version '${runtimeOverride}' must identify exactly one installed, available runtime with a ${family} Simulator device. Set it to a runtime identifier instead, or install one in Xcode Settings > Components. Inspect: xcrun simctl list runtimes --json and xcrun simctl list devices available --json.`,
      );
    runtime = matching[0];
    if (!runtime) {
      // The default is not installed (Xcode installs only its own runtimes):
      // the newest installed runtime with a device of this kind.
      const newest = usableRuntimes
        .filter((r) => hasDevice(r, family))
        .sort(newestFirst)[0];
      runtime =
        newest &&
        runtimeFor(
          family,
          usableRuntimes.filter((r) => r.version === newest.version),
        )[0];
      assert(
        runtime,
        `No installed iOS Simulator runtime has a ${family} device. Install one in Xcode Settings > Components. Inspect: xcrun simctl list runtimes --json and xcrun simctl list devices available --json.`,
      );
      console.log(
        `iOS runtime ${version} for ${family} is not installed; using the newest installed runtime with a ${family} device: ${runtime.identifier} (${runtime.version}). Set TWR_IOS_RUNTIME to choose another.`,
      );
    }
  }
  console.log(
    "IOS_SIMULATOR_RUNTIME_SELECTED",
    JSON.stringify({ kind: family, ...runtimeDescription(runtime) }),
  );
  const template = available.find(
    (device) =>
      device.runtime === runtime.identifier && deviceKind(device) === family,
  );
  assert(
    template,
    `No ${family} Simulator device template exists under requested runtime '${runtime.identifier}'. Install a compatible ${family} Simulator device in Xcode for this runtime. Inspect: xcrun simctl list devices available --json. A template from another runtime will not be substituted.`,
  );
  return { family, template, runtime };
});
const bin = resolve("../target/debug");
const execFileAsync = promisify(execFile);
mkdirSync("test-results/ios", { recursive: true });
for (const [
  index,
  { family, template, runtime: selectedRuntime },
] of selected.entries()) {
  const device = {
    ...template,
    runtime: selectedRuntime.identifier,
    udid: execFileSync(
      "xcrun",
      [
        "simctl",
        "create",
        `twr QA ${template.name}`,
        template.deviceTypeIdentifier,
        selectedRuntime.identifier,
      ],
      { encoding: "utf8" },
    ).trim(),
  };
  console.log(
    "IOS_SIMULATOR_DEVICE",
    JSON.stringify({
      kind: family,
      name: device.name,
      udid: device.udid,
      deviceTypeIdentifier: device.deviceTypeIdentifier,
      templateRuntime: template.runtime,
      runtime: runtimeDescription(selectedRuntime),
    }),
  );
  const fixture = realpathSync(mkdtempSync(join(tmpdir(), "twr-ios-")));
  const screenshot = `test-results/ios/${kind || "all"}-${index}-screen.png`;
  const performanceLabel = device.name.includes("Duo")
    ? "ios-fold"
    : device.name.includes("iPad")
      ? "ios-tablet"
      : "ios-phone";
  const performancePath = `test-results/performance-${performanceLabel}.json`;
  rmSync(performancePath, { force: true });
  const foldHelper = join(fixture, "fold-helper");
  async function fold(pose) {
    // Cold Simulator helper startup can take longer than the old 60 s settling
    // budget. Bound the whole pose below the native request's 240 s timeout,
    // and still require two independent, stable CoreMotion observations.
    const deadline = Date.now() + 210000;
    const command = async (args) => {
      const started = Date.now();
      const remaining = deadline - started;
      assert(remaining > 0, `Simulator ${pose} pose exceeded its time budget`);
      const timeoutMs = Math.min(120000, remaining);
      let result;
      try {
        result = await execFileAsync(
          "xcrun",
          ["simctl", "spawn", device.udid, foldHelper, ...args],
          { timeout: timeoutMs },
        );
      } catch (error) {
        // This helper prints only Simulator angle/pose diagnostics, never hub
        // requests or credentials. Bound output while preserving failure details.
        console.error(
          "FOLD_HELPER_FAILED",
          JSON.stringify({
            pose,
            command: args[0],
            elapsedMs: Date.now() - started,
            timeoutMs,
            code: error.code ?? null,
            signal: error.signal ?? null,
            killed: error.killed ?? null,
            stdout: String(error.stdout || "").slice(-4096),
            stderr: String(error.stderr || "").slice(-4096),
          }),
        );
        throw error;
      }
      console.log(
        "FOLD_HELPER",
        JSON.stringify({
          pose,
          command: args[0],
          elapsedMs: Date.now() - started,
        }),
      );
      return result;
    };
    await command(["set", pose]);
    let previous;
    while (Date.now() < deadline) {
      const { stdout } = await command(["hinge", "0"]);
      const reading = JSON.parse(stdout.trim().split("\n").at(-1));
      const angle = reading.angleDegrees;
      console.log("HINGE", JSON.stringify({ pose, ...reading }));
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
      ["--development", "--binary-dir", bin, "--listen", "127.0.0.1:47421"],
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
      protocol: clientProtocol,
      user_token: readFileSync(token, "utf8").trim(),
    });
    assert(hello.identity.startsWith("user:"));
    execFileSync("xcrun", ["simctl", "boot", device.udid], { stdio: "ignore" });
    execFileSync("xcrun", ["simctl", "bootstatus", device.udid, "-b"], {
      stdio: "inherit",
    });
    if (device.name.includes("Duo")) {
      // Load the helper/CoreMotion runtime before Xcode launches the test app.
      // This is only a warmup; fold() validates each requested pose separately.
      const started = Date.now();
      const { stdout } = await execFileAsync(
        "xcrun",
        ["simctl", "spawn", device.udid, foldHelper, "hinge", "0"],
        { timeout: 180000 },
      );
      const reading = JSON.parse(stdout.trim().split("\n").at(-1));
      assert(
        reading.angleValid && Number.isFinite(reading.angleDegrees),
        "Simulator CoreMotion warmup must return a valid angle",
      );
      console.log(
        "HINGE_WARMUP",
        JSON.stringify({ ...reading, elapsedMs: Date.now() - started }),
      );
    }
    const developer =
      process.env.DEVELOPER_DIR ||
      execFileSync("xcode-select", ["-p"], { encoding: "utf8" }).trim();
    const applications = join(developer, "Applications");
    if (process.env.TWR_IOS_HEADLESS !== "1" && existsSync(applications)) {
      console.log(
        "Simulator UI applications",
        readdirSync(applications).filter((name) => name.endsWith(".app")),
      );
      const ui = ["Simulator.app", "DeviceHub.app"]
        .map((name) => join(applications, name))
        .find(existsSync);
      if (ui)
        try {
          execFileSync(
            "open",
            [
              "-a",
              ui,
              ...(ui.endsWith("/Simulator.app")
                ? ["--args", "-CurrentDeviceUDID", device.udid]
                : []),
            ],
            { timeout: 30000 },
          );
        } catch (error) {
          console.log("Simulator UI could not open:", error.message);
        }
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
              url: "http://127.0.0.1:47424",
              foldable: device.name.includes("Duo"),
              catalogIds: JSON.parse(
                readFileSync(
                  new URL("../catalog.json", import.meta.url),
                  "utf8",
                ),
              ).map((command) => command.id),
            }),
          );
        } catch (error) {
          res.statusCode = 500;
          res.end(JSON.stringify({ error: String(error) }));
        }
      } else if (req.url.startsWith("/screenshot?") && req.method === "POST") {
        const name = new URL(req.url, "http://localhost").searchParams.get(
          "name",
        );
        if (!/^[a-z-]{1,40}$/.test(name || "")) {
          res.statusCode = 400;
          res.end("Invalid snapshot name");
          return;
        }
        const chunks = [];
        let bytes = 0;
        for await (const chunk of req) {
          bytes += chunk.length;
          if (bytes > 16 * 1024 * 1024) {
            res.statusCode = 413;
            res.end("Snapshot too large");
            return;
          }
          chunks.push(chunk);
        }
        const png = Buffer.concat(chunks);
        if (
          !png
            .subarray(0, 8)
            .equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))
        ) {
          res.statusCode = 400;
          res.end("Invalid PNG");
          return;
        }
        writeFileSync(
          `test-results/ios/${kind || "all"}-${index}-${name}.png`,
          png,
        );
        res.end("ok");
      } else if (req.url === "/drop-socket") {
        for (const socket of proxySockets) socket.destroy();
        res.end("ok");
      } else if (req.url === "/typography-script") {
        res.end(
          readFileSync(new URL("./connected-typography.js", import.meta.url)),
        );
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
            ...qaSource(),
            label: performanceLabel,
            simulatorDevice: device.name,
            simulatorRuntime: device.runtime,
            qaVerified: false,
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
    await new Promise((r) => server.listen(47423, "127.0.0.1", r));
    proxy = createProxy((socket) => {
      const upstream = connectTCP(47421, "127.0.0.1");
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
    await new Promise((resolve) => proxy.listen(47424, "127.0.0.1", resolve));
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
        "-packageAuthorizationProvider",
        "netrc",
        "-disableAutomaticPackageResolution",
        "-onlyUsePackageVersionsFromResolvedFile",
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
    if (!existsSync(screenshot)) {
      console.warn(
        "Friendly PNG export missing; native app snapshots are retained in the XCTest result bundle.",
      );
      if (exit !== 0) {
        try {
          execFileSync(
            "xcrun",
            [
              "simctl",
              "io",
              device.udid,
              "screenshot",
              screenshot.replace(/screen\.png$/, "failure-display.png"),
            ],
            { stdio: "inherit", timeout: 15000 },
          );
        } catch (error) {
          console.warn(
            "Could not capture the final Simulator display:",
            error.message,
          );
        }
      }
    }
    assert.equal(exit, 0, `${device.name} native integration tests must pass`);
    if (existsSync(performancePath)) {
      const report = JSON.parse(readFileSync(performancePath, "utf8"));
      writeFileSync(
        performancePath,
        JSON.stringify({ ...report, qaVerified: true }, null, 2) + "\n",
      );
    }
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
    stopFixture(join(bin, "twr"), env);
    rmSync(fixture, { recursive: true, force: true });
  }
}
