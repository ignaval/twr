import { _android, expect } from "@playwright/test";
import { spawn, execFileSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  existsSync,
  realpathSync,
  readdirSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import assert from "node:assert/strict";
import { performanceAudit } from "../../scripts/performance.mjs";
import { terminalUIQA } from "./terminal-ui-qa.mjs";
import { multiHubQA } from "./multi-hub-qa.mjs";
import { systemPickerQA } from "./system-picker-qa.mjs";
const adb =
  process.env.TWR_ADB ||
  join(process.env.HOME, "Library/Android/sdk/platform-tools/adb");
const serial = process.env.TWR_ANDROID_SERIAL || "emulator-5554";
const adbRun = (args, options = {}) =>
  execFileSync(adb, ["-s", serial, ...args], { encoding: "utf8", ...options });
const fixture = mkdtempSync(join(tmpdir(), "twr-mobile-"));
const repo = join(fixture, "repo");
mkdirSync(repo);
execFileSync("git", ["init", "-b", "main", repo], { stdio: "ignore" });
execFileSync(
  "git",
  [
    "-C",
    repo,
    "-c",
    "user.name=Mobile QA",
    "-c",
    "user.email=mobile@example.invalid",
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
const bin = resolve("../target/debug");
let gateway;
let fixtureHub;
let device;
let devices = [];
let page;
let hubPid;
let hubPaused = false;
const port = 47321;
const cdp = 47322;
let stderr = "";
let stdout = "";
try {
  fixtureHub = spawn(join(bin, "twr"), ["hub"], { env, stdio: "ignore" });
  for (
    let attempt = 0;
    attempt < 150 && !existsSync(join(fixture, "hub.sock"));
    attempt++
  ) {
    assert(fixtureHub.exitCode === null, "Fixture hub must start");
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
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
      `127.0.0.1:${port}`,
    ],
    { env, stdio: ["ignore", "pipe", "pipe"] },
  );
  gateway.stdout.on("data", (d) => (stdout += d));
  gateway.stderr.on("data", (d) => (stderr += d));
  for (let i = 0; i < 150 && !stdout.includes("Pairing code"); i++) {
    assert(gateway.exitCode === null, stderr);
    await new Promise((r) => setTimeout(r, 200));
  }
  const code = stdout.match(/Pairing code[^:]+: (\w+)/)?.[1];
  assert(code, "Gateway should provide a one-use pairing code: " + stderr);
  adbRun([
    "install",
    "-r",
    "mobile/android/app/build/outputs/apk/debug/app-debug.apk",
  ]);
  adbRun(["reverse", `tcp:${port}`, `tcp:${port}`]);
  adbRun(["shell", "am", "force-stop", "tools.twr.mobile.dev"]);
  adbRun(["shell", "pm", "clear", "tools.twr.mobile.dev"]);
  adbRun(["shell", "am", "start", "-n", "tools.twr.mobile.dev/.MainActivity"]);
  devices = await _android.devices({ omitDriverInstall: true });
  device = devices.find((d) => d.serial() === serial);
  assert(device, "An Android emulator must be running");
  const webview = await device.webView({ pkg: "tools.twr.mobile.dev" });
  page = await webview.page();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.getByRole("button", { name: "Pair a hub", exact: true }).click();
  await page.getByLabel("Hub name", { exact: true }).fill("QA workstation");
  await page
    .getByLabel("Hub URL", { exact: true })
    .fill(`http://127.0.0.1:${port}`);
  await page.getByLabel("Pairing code", { exact: true }).fill(code);
  await page.getByLabel("This device", { exact: true }).fill("Android QA");
  await page
    .getByRole("button", { name: "Pair and connect", exact: true })
    .click();
  await expect
    .poll(
      () =>
        page.evaluate(async () => {
          try {
            return (await window.twr.rpc("hub.info")).identity;
          } catch {
            return "";
          }
        }),
      { timeout: 30000 },
    )
    .toMatch(/^user:device:/);
  hubPid = (await page.evaluate(() => window.twr.rpc("hub.info"))).pid;
  const metadata = await page.evaluate(() =>
    window.Capacitor.Plugins.TwrTransport.listHubs(),
  );
  assert(
    !JSON.stringify(metadata).includes('"token"'),
    "Credentials must not cross the JS bridge",
  );
  console.log(
    "PASS: native Android pairing, KeyStore storage, authenticated remote hub and shared UI",
  );
  await page.evaluate(() =>
    window.twr.rpc("thread.new", {
      project: "demo",
      name: "android-worker",
      runtime: "fake",
    }),
  );
  const threadsTab = page
    .locator(".mobile-tabs")
    .getByRole("button", { name: "Threads", exact: true });
  if (await threadsTab.isVisible()) await threadsTab.click();
  else
    await page
      .locator(".primary-nav")
      .getByRole("button", { name: /^Thread grid/ })
      .click();
  await page
    .locator(".thread-grid .thread-card")
    .filter({ hasText: "android-worker" })
    .click();
  await page
    .getByRole("button", { name: "Talk to this thread", exact: true })
    .click();
  let input = page.getByRole("textbox", {
    name: "Message android-worker directly",
    exact: true,
  });
  await input.fill("ANDROID_REMOTE_STREAM_OK");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  await page
    .locator(".block-assistant")
    .getByText("ANDROID_REMOTE_STREAM_OK", { exact: true })
    .waitFor();
  const identity = (await page.evaluate(() => window.twr.rpc("hub.info")))
    .identity;
  assert(identity.startsWith("user:device:"));
  console.log(
    "PASS: device audit identity, direct destination and remote transcript streaming",
  );
  await page.getByLabel("Upload files", { exact: true }).setInputFiles({
    name: "qa-attachment.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("ANDROID_FILE_UPLOAD_OK"),
  });
  await expect
    .poll(() => input.inputValue(), { timeout: 20000 })
    .toContain(".twr-uploads/");
  const uploaded = (await input.inputValue()).trim().split("\n").at(-1);
  assert.equal(readFileSync(uploaded, "utf8"), "ANDROID_FILE_UPLOAD_OK");
  const thread = await page.evaluate(() =>
    window.twr.rpc("thread.get", { thread: "android-worker" }),
  );
  assert(
    !execFileSync("git", ["-C", thread.worktree, "status", "--porcelain"], {
      encoding: "utf8",
    }).includes(".twr-uploads"),
  );
  await input.fill("");
  console.log(
    "PASS: native file picker upload, checksum, working-directory placement and git exclusion",
  );
  if (process.env.TWR_SYSTEM_PICKER_QA === "1") {
    const next = await systemPickerQA({
      page,
      devices,
      adbRun,
      fixture,
      serial,
    });
    page = next.page;
    devices = next.devices;
    device = next.device;
    input = page.getByRole("textbox", {
      name: "Message android-worker directly",
      exact: true,
    });
    await input.fill("");
    page.on("pageerror", (e) => errors.push(e.message));
  }
  if (serial === "emulator-5556") {
    await input.fill("FOLD_TRANSITION_DRAFT");
    await page.evaluate(() => window.Capacitor.Plugins.Keyboard.hide());
    const width = await page.evaluate(() => innerWidth);
    adbRun(["shell", "cmd", "device_state", "state", "0"]);
    await expect
      .poll(() => page.evaluate(() => innerWidth), { timeout: 15000 })
      .not.toBe(width);
    assert.equal(await input.inputValue(), "FOLD_TRANSITION_DRAFT");
    adbRun(["shell", "cmd", "device_state", "state", "2"]);
    await expect
      .poll(() => page.evaluate(() => innerWidth), { timeout: 15000 })
      .toBe(width);
    assert.equal(await input.inputValue(), "FOLD_TRANSITION_DRAFT");
    adbRun(["shell", "cmd", "device_state", "state", "1"]);
    await expect
      .poll(
        () =>
          page
            .evaluate(() => window.Capacitor.Plugins.TwrLayout.current())
            .then((p) => p.state),
        { timeout: 15000 },
      )
      .toBe("half-opened");
    await expect
      .poll(() => page.evaluate(() => document.documentElement.dataset.fold))
      .toBe("vertical");
    assert.equal(await input.inputValue(), "FOLD_TRANSITION_DRAFT");
    adbRun([
      "shell",
      "settings",
      "put",
      "system",
      "accelerometer_rotation",
      "0",
    ]);
    adbRun(["shell", "settings", "put", "system", "user_rotation", "1"]);
    await expect
      .poll(
        () =>
          page
            .evaluate(() => window.Capacitor.Plugins.TwrLayout.current())
            .then((p) => p.orientation),
        { timeout: 15000 },
      )
      .toBe("horizontal");
    assert.equal(await input.inputValue(), "FOLD_TRANSITION_DRAFT");
    const foldGeometry = await page.evaluate(() => {
      const composer = document
        .querySelector(".composer")
        .getBoundingClientRect();
      const top = parseFloat(
        getComputedStyle(document.documentElement).getPropertyValue(
          "--hinge-top",
        ),
      );
      const height = parseFloat(
        getComputedStyle(document.documentElement).getPropertyValue(
          "--hinge-height",
        ),
      );
      return { composerTop: composer.top, foldBottom: top + height };
    });
    assert(
      foldGeometry.composerTop >= foldGeometry.foldBottom - 2,
      JSON.stringify(foldGeometry),
    );
    adbRun(["shell", "settings", "put", "system", "user_rotation", "0"]);
    adbRun(["shell", "cmd", "device_state", "state", "2"]);
    await expect
      .poll(() => page.evaluate(() => innerWidth), { timeout: 15000 })
      .toBe(width);
    await expect
      .poll(() => page.evaluate(() => document.documentElement.dataset.fold))
      .toBe("none");
    await input.fill("");
    console.log(
      "PASS: folded cover screen, unfolded book and half-open hinge transitions preserve drafts",
    );
  }
  await page.evaluate(() =>
    window.twr.rpc("thread.send", {
      thread: "android-worker",
      prompt:
        "!ask Continue Android QA?|Proceed|Wait\nANDROID_QUESTION_ANSWERED",
    }),
  );
  const needsTab = page
    .locator(".mobile-tabs")
    .getByRole("button", { name: /Needs you/ });
  if (await needsTab.isVisible()) await needsTab.click();
  else {
    const show = page.getByRole("button", {
      name: "Show sidebar",
      exact: true,
    });
    if (await show.isVisible()) await show.click();
    await page
      .locator(".primary-nav")
      .getByRole("button", { name: /Needs you/ })
      .click();
  }
  const question = page
    .locator(".attention-card")
    .filter({ hasText: "Continue Android QA?" });
  await question.getByRole("button", { name: /Proceed/ }).click();
  await question.waitFor({ state: "hidden" });
  assert.equal((await page.evaluate(() => window.twr.catalog())).length, 71);
  console.log(
    "PASS: mobile attention response and complete shared CLI catalog",
  );
  const terminal = await page.evaluate(() =>
    window.twr.terminalOpen({ shell: true, title: "Android shell" }),
  );
  await page.evaluate(
    (id) => window.twr.terminalWrite(id, "printf 'ANDROID_REMOTE_PTY_OK\\n'\r"),
    terminal.id,
  );
  await expect
    .poll(
      () =>
        page
          .evaluate((id) => window.twr.terminalRead(id), terminal.id)
          .then((r) => r.data),
      { timeout: 10000 },
    )
    .toContain("ANDROID_REMOTE_PTY_OK");
  await page.evaluate((id) => {
    window.__qaTerminal = id;
  }, terminal.id);
  // A lost socket must reconnect on foregrounding without losing its PTY.
  await page.evaluate(() => window.Capacitor.Plugins.TwrTransport.disconnect());
  adbRun(["shell", "input", "keyevent", "KEYCODE_HOME"]);
  adbRun(["shell", "am", "start", "-n", "tools.twr.mobile.dev/.MainActivity"]);
  await expect
    .poll(
      () =>
        page.evaluate(async () => {
          try {
            return (await window.twr.terminalRead(window.__qaTerminal)).data;
          } catch {
            return "";
          }
        }),
      { timeout: 30000 },
    )
    .toContain("ANDROID_REMOTE_PTY_OK");
  console.log(
    "PASS: foreground reconnect retains the remote terminal and history",
  );
  await page.evaluate((id) => {
    const command =
      "printf 'ORDER_%s\\n' '0123456789abcdefghijklmnopqrstuvwxyz'\r";
    for (const char of command) window.twr.terminalWrite(id, char);
  }, terminal.id);
  await expect
    .poll(
      () =>
        page
          .evaluate((id) => window.twr.terminalRead(id), terminal.id)
          .then((r) => r.data),
      { timeout: 15000 },
    )
    .toContain("ORDER_0123456789abcdefghijklmnopqrstuvwxyz");
  await page.evaluate(() => {
    window.__qaDrops = 0;
    window.__qaDropListener = window.twr.onEvent((e) => {
      if (e.type === "connection" && !e.connected) window.__qaDrops++;
    });
  });
  await page.evaluate(
    (id) =>
      window.twr.terminalWrite(
        id,
        "head -c 3145728 /dev/zero | tr '\\000' x; printf '\\nHEAVY_PTY_FINISHED\\n'\r",
      ),
    terminal.id,
  );
  await expect
    .poll(
      () =>
        page
          .evaluate((id) => window.twr.terminalRead(id), terminal.id)
          .then(
            (r) =>
              r.data.endsWith("HEAVY_PTY_FINISHED\r\n") ||
              r.data.includes("HEAVY_PTY_FINISHED\r\n"),
          ),
      { timeout: 30000 },
    )
    .toBe(true);
  assert.equal(
    await page.evaluate(() => {
      window.__qaDropListener();
      return window.__qaDrops;
    }),
    0,
  );
  console.log(
    "PASS: ordered keystrokes and 3 MiB terminal output without a full reconnect",
  );
  await page.evaluate((id) => window.twr.terminalClose(id), terminal.id);
  console.log("PASS: Android to remote native PTY input and output");
  await terminalUIQA(page);
  const pickedDirectory = page.evaluate(() => window.twr.pickDirectory());
  const directoryDialog = page.getByRole("dialog", {
    name: "Directory on your hub",
    exact: true,
  });
  await directoryDialog
    .getByLabel("Absolute directory", { exact: true })
    .fill(repo);
  await directoryDialog
    .getByRole("button", { name: "Browse", exact: true })
    .click();
  await directoryDialog
    .getByRole("button", { name: "Use directory", exact: true })
    .click();
  assert.equal(await pickedDirectory, realpathSync(repo));
  console.log("PASS: remote directory browsing and explicit folder selection");

  const recoveryParams = {
    project: "demo",
    name: "recovered-once",
    runtime: "fake",
  };
  const canonical = (value) =>
    Array.isArray(value)
      ? value.map(canonical)
      : value && typeof value === "object"
        ? Object.fromEntries(
            Object.keys(value)
              .sort()
              .map((key) => [key, canonical(value[key])]),
          )
        : value;
  const recoveryHash = createHash("sha256")
    .update(
      JSON.stringify(
        canonical([
          "default",
          "rpc",
          { method: "thread.new", params: recoveryParams },
        ]),
      ),
    )
    .digest("hex");
  process.kill(hubPid, "SIGSTOP");
  hubPaused = true;
  await page.evaluate((params) => {
    window.__qaRecovered = undefined;
    void window.twr.rpc("thread.new", params).then(
      (result) => (window.__qaRecovered = { result }),
      (error) => (window.__qaRecovered = { error: String(error) }),
    );
  }, recoveryParams);
  const receipts = join(fixture, "data", "mobile", "requests");
  await expect
    .poll(() =>
      readdirSync(receipts).some((name) => {
        try {
          return (
            JSON.parse(readFileSync(join(receipts, name), "utf8")).hash ===
            recoveryHash
          );
        } catch {
          return false;
        }
      }),
    )
    .toBe(true);
  await page.evaluate(() => window.Capacitor.Plugins.TwrTransport.disconnect());
  adbRun(["shell", "input", "keyevent", "KEYCODE_HOME"]);
  process.kill(hubPid, "SIGCONT");
  hubPaused = false;
  adbRun(["shell", "am", "start", "-n", "tools.twr.mobile.dev/.MainActivity"]);
  await expect
    .poll(() => page.evaluate(() => window.__qaRecovered), { timeout: 30000 })
    .toBeTruthy();
  const recovered = await page.evaluate(() => window.__qaRecovered);
  assert(!recovered.error, JSON.stringify(recovered));
  assert.equal(
    (await page.evaluate(() => window.twr.rpc("thread.list"))).filter(
      (t) => t.name === "recovered-once",
    ).length,
    1,
  );
  console.log(
    "PASS: a mutation accepted before disconnect recovers its original reply and executes once",
  );
  const duplicate = await page.evaluate(async () => {
    const id = crypto.randomUUID();
    const message = JSON.stringify({
      id,
      method: "rpc",
      params: {
        method: "thread.new",
        params: { project: "demo", name: "receipt-once", runtime: "fake" },
      },
    });
    async function call() {
      return new Promise(async (resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error("Receipt response timed out")),
          10000,
        );
        const handle = await window.Capacitor.Plugins.TwrTransport.addListener(
          "message",
          ({ message: reply }) => {
            if (reply.id === id) {
              clearTimeout(timer);
              void handle.remove();
              resolve(reply);
            }
          },
        );
        await window.Capacitor.Plugins.TwrTransport.send({ message });
      });
    }
    return [await call(), await call()];
  });
  assert.deepEqual(duplicate[0], duplicate[1]);
  assert(!duplicate[0].error, JSON.stringify(duplicate[0]));
  console.log(
    "PASS: duplicate mutation ID replays the durable result without creating another thread",
  );
  if (serial === "emulator-5554") {
    adbRun([
      "shell",
      "pm",
      "grant",
      "tools.twr.mobile.dev",
      "android.permission.POST_NOTIFICATIONS",
    ]);
    await page
      .getByRole("button", { name: "Remote hub settings", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Test notification", exact: true })
      .click();
    await expect(page.getByRole("status")).toContainText(
      "Local notification sent.",
    );
    await expect
      .poll(() =>
        page.evaluate(async () => {
          const result =
            await window.Capacitor.Plugins.LocalNotifications.getDeliveredNotifications();
          return result.notifications.some(
            (n) => n.title === "twr · local test",
          );
        }),
      )
      .toBe(true);
    adbRun(["shell", "cmd", "statusbar", "expand-notifications"]);
    let tapped = false;
    for (let attempt = 0; attempt < 10; attempt++) {
      adbRun(["shell", "uiautomator", "dump", "/sdcard/twr-qa-ui.xml"]);
      const xml = adbRun(["shell", "cat", "/sdcard/twr-qa-ui.xml"]);
      const node = [...xml.matchAll(/<node\b[^>]*>/g)]
        .map((m) => m[0])
        .find((n) => n.includes('text="twr · local test"'));
      const rect = node
        ?.match(/bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"/)
        ?.slice(1)
        .map(Number);
      if (rect) {
        adbRun([
          "shell",
          "input",
          "tap",
          String(Math.round((rect[0] + rect[2]) / 2)),
          String(Math.round((rect[1] + rect[3]) / 2)),
        ]);
        tapped = true;
        break;
      }
    }
    assert(
      tapped,
      "Native notification must be visible in Android's system shade",
    );
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(
      page.locator(".mobile-tabs").getByRole("button", { name: /Needs you/ }),
    ).toHaveAttribute("aria-current", "page");
    console.log(
      "PASS: real Android system notification delivery and tap routing to Needs you",
    );
  }
  if (process.env.TWR_PERF === "1")
    await performanceAudit(
      page,
      serial === "emulator-5556" ? "android-fold" : "android-phone",
    );
  mkdirSync("test-results", { recursive: true });
  writeFileSync(
    serial === "emulator-5556"
      ? "test-results/android-fold-qa.png"
      : "test-results/android-qa.png",
    (() => {
      const raw = adbRun(["exec-out", "screencap", "-p"], { encoding: null });
      const start = raw.indexOf(Buffer.from("89504e470d0a1a0a", "hex"));
      assert(start >= 0, "screencap must contain a PNG image");
      return raw.subarray(start);
    })(),
  );
  if (serial === "emulator-5554") {
    await multiHubQA({
      page,
      fixture,
      repo,
      env,
      bin,
      adbRun,
      originalGateway: metadata.hubs[0].gateway_id,
    });
    // Exercise the owner's default read + operate grant, with no admin/terminal.
    const pairOutput = execFileSync(
      join(bin, "twr-mobile-gateway"),
      ["--pair", "--pair-only", "--binary-dir", bin],
      { env, encoding: "utf8" },
    );
    const limitedCode = pairOutput.match(/Pairing code[^:]+: (\w+)/)?.[1];
    assert(limitedCode);
    await page
      .getByRole("button", { name: "Remote hub settings", exact: true })
      .click();
    await page.getByRole("button", { name: "Switch hub", exact: true }).click();
    await page.getByRole("button", { name: /Pair.*hub/i }).click();
    await page
      .getByLabel("Hub name", { exact: true })
      .fill("Limited access QA");
    await page
      .getByLabel("Hub URL", { exact: true })
      .fill(`http://127.0.0.1:${port}`);
    await page.getByLabel("Pairing code", { exact: true }).fill(limitedCode);
    await page.getByLabel("This device", { exact: true }).fill("Limited QA");
    await page
      .getByRole("button", { name: "Pair and connect", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Open shell", exact: true }),
    ).toBeDisabled();
    await page
      .locator(".mobile-tabs")
      .getByRole("button", { name: "More", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Accounts", exact: true }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "Boxes", exact: true }),
    ).toHaveCount(0);
    console.log(
      "PASS: default device scopes disable terminals and hide admin sections",
    );
  }
  assert.deepEqual(errors, []);
  console.log("PASS: Android phone QA");
} catch (error) {
  mkdirSync("test-results", { recursive: true });
  writeFileSync("test-results/android-gateway.log", stderr);
  if (page) {
    console.error(await page?.locator("body").innerText());
    await page
      ?.screenshot({ path: "test-results/android-failure.png" })
      .catch(() => {});
  }
  throw error;
} finally {
  await Promise.all(devices.map((d) => d.close().catch(() => {})));
  try {
    adbRun(["shell", "am", "force-stop", "tools.twr.mobile.dev"]);
  } catch {}
  if (hubPaused)
    try {
      process.kill(hubPid, "SIGCONT");
    } catch {}
  gateway?.kill();
  fixtureHub?.kill();
  if (hubPid) {
    try {
      process.kill(hubPid, "SIGTERM");
    } catch {}
  }
  await new Promise((r) => setTimeout(r, 1500));
  rmSync(fixture, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}
