// Launch the actual release app through LaunchServices so macOS supplies a real user
// process ancestry. This exercises production authentication, not the debug test token.
import { performanceAudit } from "./performance.mjs";
import { chromium } from "@playwright/test";
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  existsSync,
  rmSync,
  readFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";

if (process.platform !== "darwin" || process.arch !== "arm64")
  throw new Error(
    "This packaged launcher smoke test runs on Apple Silicon macOS.",
  );
const application = resolve("release/mac-arm64/twr.app");
assert(existsSync(application), "Build the Mac application first");
const fixture = mkdtempSync(join(tmpdir(), "twr-package-"));
const repository = join(fixture, "repo");
const speechScript = join(fixture, "speech-sink.sh");
const spoken = join(fixture, "spoken.txt");
writeFileSync(speechScript, 'cat >> "$1"\n', { mode: 0o600 });
mkdirSync(repository);
execFileSync("git", ["init", "-b", "main", repository], { stdio: "ignore" });
execFileSync(
  "git",
  [
    "-C",
    repository,
    "-c",
    "user.name=Desktop Test",
    "-c",
    "user.email=desktop@example.invalid",
    "commit",
    "--allow-empty",
    "-m",
    "Initial",
  ],
  { stdio: "ignore" },
);
writeFileSync(
  join(fixture, "config.toml"),
  `default_runtime = "fake"\n[projects.demo]\nrepo = ${JSON.stringify(repository)}\n[orchestrator]\nruntime = "fake"\nautostart = false\n[tui]\nsuggest = false\nsummaries = false\n[speech]\nengine = "command"\ncommand = ${JSON.stringify(["/bin/sh", speechScript, spoken])}\n`,
  { mode: 0o600 },
);
const port = await new Promise((resolvePort, reject) => {
  const socket = createServer();
  socket.on("error", reject);
  socket.listen(0, "127.0.0.1", () => {
    const port = socket.address().port;
    socket.close(() => resolvePort(port));
  });
});
let browser;
let hubPid;
const hubPids = new Set();
try {
  const launchedAt = performance.now();
  execFileSync("/usr/bin/open", [
    "-n",
    "-a",
    application,
    "--stdout",
    join(fixture, "app.stdout"),
    "--stderr",
    join(fixture, "app.stderr"),
    "--env",
    `TWR_HOME=${fixture}`,
    "--env",
    `HOME=${fixture}`,
    "--env",
    "TWR_ENABLE_FAKE=1",
    "--env",
    "TWR_DESKTOP_TEST_AUTOMATION=1",
    "--env",
    "TWR_WORKSPACE=default",
    "--env",
    "SHELL=/bin/sh",
    "--env",
    "PATH=/usr/bin:/bin",
    "--env",
    "ELECTRON_RUN_AS_NODE",
    "--env",
    "NODE_OPTIONS",
    "--args",
    `--user-data-dir=${join(fixture, "user-data")}`,
    `--remote-debugging-port=${port}`,
  ]);
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`, {
        timeout: 1000,
      });
      break;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
  assert(
    browser,
    "Packaged application did not expose its test debugging port",
  );
  const page = browser.contexts()[0].pages()[0];
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 60000 });
  mkdirSync("test-results", { recursive: true });
  writeFileSync(
    "test-results/packaged-startup.json",
    JSON.stringify(
      {
        platform: "macos-arm64",
        launchToHubConnectedMs: Math.round(performance.now() - launchedAt),
        coldFixture: true,
        date: new Date().toISOString(),
      },
      null,
      2,
    ) + "\n",
  );
  const boot = await page.evaluate(() => window.twr.bootstrap());
  assert.equal(boot.home, fixture);
  const info = await page.evaluate(() => window.twr.rpc("hub.info"));
  hubPid = info.pid;
  hubPids.add(hubPid);
  assert(
    Number.isInteger(hubPid) && hubPid > 1,
    "Hub PID required for isolated cleanup",
  );
  assert(
    existsSync(join(fixture, "user-data", "engines")),
    "Engine pair must be cached outside the app bundle",
  );
  await page.evaluate(() =>
    window.twr.rpc("thread.new", {
      project: "demo",
      name: "packaged",
      runtime: "fake",
    }),
  );
  await page
    .locator(".thread-card")
    .filter({ hasText: "packaged" })
    .first()
    .click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await page
    .getByRole("textbox", { name: "Message packaged directly" })
    .fill("PACKAGED_AGENT_OK");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  await page
    .locator(".block-assistant")
    .getByText("PACKAGED_AGENT_OK", { exact: true })
    .waitFor();
  if (process.env.TWR_DESKTOP_EXTENDED_QA === "1") {
    const { runMacQA } = await import("./qa-scenarios.mjs");
    await runMacQA(page, { fixture, hubPids });
  }
  // Exercise native menu shortcuts while xterm owns keyboard focus.
  await page.getByRole("button", { name: "Open shell", exact: true }).click();
  const uiTerminal = page.locator(".terminal-session:visible");
  const uiTerminalId = await uiTerminal.getAttribute("data-session-id");
  const uiKeyboard = uiTerminal.locator(".xterm-helper-textarea");
  await page.evaluate(() => window.twr.copyText("printf 'MAC_PASTE_%s\\n' OK"));
  await uiKeyboard.focus();
  await uiKeyboard.press("Meta+v");
  await uiKeyboard.press("Enter");
  await page.waitForFunction(
    async (id) =>
      (await window.twr.terminalRead(id)).data.includes("MAC_PASTE_OK\r\n"),
    uiTerminalId,
  );
  await uiKeyboard.press("Meta+a");
  await uiKeyboard.press("Meta+c");
  assert(
    execFileSync("/usr/bin/pbpaste", { encoding: "utf8" }).includes(
      "MAC_PASTE_OK",
    ),
    "Cmd-C must copy the selected terminal output",
  );
  console.log(
    "QA: native Cmd-V/C paste and copy work while the embedded terminal owns keyboard focus",
  );
  await page.getByRole("button", { name: "Close Shell", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Close terminal?", exact: true })
    .getByRole("button", { name: "Continue", exact: true })
    .click();
  const terminal = await page.evaluate(() =>
    window.twr.terminalOpen({ shell: true, title: "Packaged terminal" }),
  );
  await page.evaluate(
    (id) => window.twr.terminalWrite(id, "printf 'PACKAGED_PTY_OK\\n'\r"),
    terminal.id,
  );
  for (let attempt = 0; attempt < 100; attempt++) {
    const output = await page.evaluate(
      (id) => window.twr.terminalRead(id),
      terminal.id,
    );
    if (output.data.includes("PACKAGED_PTY_OK\r\n")) break;
    if (attempt === 99)
      throw new Error("Packaged native terminal did not produce output");
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  await page.evaluate((id) => window.twr.terminalClose(id), terminal.id);
  mkdirSync("test-results", { recursive: true });
  if (process.env.TWR_PERF === "1")
    await performanceAudit(page, "mac-packaged");
  await page.screenshot({ path: "test-results/packaged-macos.png" });
  if (process.env.TWR_UI_CAPTURES === "1") {
    const { captureUI } = await import("./ui-captures.mjs");
    await captureUI(page, { hubPids });
  }
  await page.evaluate(() => window.twr.quit()).catch(() => {});
  process.kill(hubPid, 0);
  console.log(
    "PASS: packaged Apple Silicon app, production user authentication, bundled engine startup, first-turn streaming, native PTY and independent hub lifetime.",
  );
} catch (error) {
  if (existsSync(join(fixture, "data/hub.log"))) {
    mkdirSync("test-results", { recursive: true });
    writeFileSync(
      "test-results/mac-qa-hub.log",
      readFileSync(join(fixture, "data/hub.log")),
    );
  }
  if (browser?.isConnected()) {
    const page = browser.contexts()[0]?.pages()[0];
    if (page) {
      console.error(
        "QA window:",
        await page
          .locator("body")
          .innerText()
          .catch(() => "unavailable"),
      );
      mkdirSync("test-results", { recursive: true });
      await page
        .screenshot({ path: "test-results/mac-qa-failure.png" })
        .catch(() => {});
    }
  }
  for (const name of ["app.stdout", "app.stderr"])
    if (existsSync(join(fixture, name)))
      console.error(
        name,
        readFileSync(join(fixture, name), "utf8").slice(-10000),
      );
  throw error;
} finally {
  if (browser) {
    await Promise.race([
      Promise.all(
        browser
          .contexts()
          .flatMap((context) => context.pages())
          .map((page) =>
            page.evaluate(() => window.twr?.quit()).catch(() => {}),
          ),
      ),
      new Promise((resolve) => setTimeout(resolve, 1000)),
    ]);
    // A failing draft-retention test may leave a native discard dialog open. Identify
    // only this fixture's launcher process, never another twr window, and end it.
    for (const line of execFileSync("/bin/ps", ["-axo", "pid=,command="], {
      encoding: "utf8",
    }).split("\n")) {
      const match = line.trim().match(/^(\d+) (.+)$/);
      if (
        match &&
        match[2].startsWith(application + "/Contents/MacOS/twr ") &&
        match[2].includes(`--user-data-dir=${join(fixture, "user-data")}`) &&
        match[2].includes(`--remote-debugging-port=${port}`)
      ) {
        try {
          process.kill(Number(match[1]), "SIGKILL");
        } catch {}
      }
    }
    await browser.close().catch(() => {});
  }
  for (const hubPid of hubPids) {
    try {
      process.kill(hubPid, "SIGTERM");
    } catch {}
    for (let attempt = 0; attempt < 50; attempt++) {
      try {
        process.kill(hubPid, 0);
      } catch {
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  rmSync(fixture, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}
