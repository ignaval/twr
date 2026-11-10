// Launch the actual release app through LaunchServices so macOS supplies a real user
// process ancestry. This exercises production authentication, not the debug test token.
import { performanceAudit } from "./performance.mjs";
import { chromium } from "@playwright/test";
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  realpathSync,
  mkdirSync,
  writeFileSync,
  existsSync,
  rmSync,
  readFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

if (process.platform !== "darwin" || process.arch !== "arm64")
  throw new Error(
    "This packaged launcher smoke test runs on Apple Silicon macOS.",
  );
const release = resolve("release/mac-arm64/twr.app");
assert(existsSync(release), "Build the Mac application first");
const resources = join(release, "Contents/Resources");
// A packaged build honours TWR_DESKTOP_TEST_AUTOMATION only when its bundle carries this
// marker (desktop/automation.ts). Release packaging never writes it: this test checks that
// the release app ignores the variable, then drives a copy of it that has the marker.
const QA_MARKER = "twr-qa-automation";
assert(
  !existsSync(join(resources, QA_MARKER)),
  `The release app must not carry the QA marker ${QA_MARKER}`,
);
// The menu bar icon and electron-updater's feed configuration ship in the bundle.
for (const name of ["trayTemplate.png", "trayTemplate@2x.png"])
  assert(
    existsSync(join(resources, "tray", name)),
    `Missing menu bar icon ${name}`,
  );
{
  const feed = readFileSync(join(resources, "app-update.yml"), "utf8");
  for (const line of ["provider: github", "owner: ignaval", "repo: twr"])
    assert(feed.split("\n").includes(line), `app-update.yml lacks ${line}`);
}
// twr's licence and the third-party notices (scripts/notices.mjs) the Help menu opens.
for (const name of ["LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html"])
  assert(existsSync(join(resources, name)), `The app bundle has no ${name}`);
{
  const notices = readFileSync(
    join(resources, "THIRD-PARTY-LICENSES.html"),
    "utf8",
  );
  for (const name of ["react", "@xterm/xterm", "electron-updater", "node-pty"])
    assert(
      notices.includes(`data-package="${name}"`),
      `THIRD-PARTY-LICENSES.html does not list ${name}`,
    );
  if (!notices.includes('id="rust-crates"'))
    console.log(
      "WARNING: THIRD-PARTY-LICENSES.html lists no Rust crates: it was packaged without cargo-about",
    );
}
const fixture = realpathSync(mkdtempSync(join(tmpdir(), "twr-package-")));
function launcherPID(app, userData, port) {
  for (const line of execFileSync("/bin/ps", ["-axo", "pid=,command="], {
    encoding: "utf8",
  }).split("\n")) {
    const match = line.trim().match(/^(\d+) (.+)$/);
    if (
      match &&
      match[2].startsWith(app + "/Contents/MacOS/twr ") &&
      match[2].includes(`--user-data-dir=${userData}`) &&
      match[2].includes(`--remote-debugging-port=${port}`)
    )
      return Number(match[1]);
  }
  return undefined;
}
async function freePort() {
  return await new Promise((resolvePort, reject) => {
    const socket = createServer();
    socket.on("error", reject);
    socket.listen(0, "127.0.0.1", () => {
      const port = socket.address().port;
      socket.close(() => resolvePort(port));
    });
  });
}
// Without the marker, the release app must start normally and keep its debugging port shut.
{
  const home = join(fixture, "unmarked");
  mkdirSync(home);
  writeFileSync(
    join(home, "config.toml"),
    `[orchestrator]\nautostart = false\n[tui]\nsuggest = false\nsummaries = false\n`,
    { mode: 0o600 },
  );
  const userData = join(home, "user-data");
  const port = await freePort();
  const twr = join(resources, "bin/twr");
  const hubEnv = { ...process.env, HOME: home, TWR_HOME: home };
  delete hubEnv.TWR_DESKTOP_TEST_AUTOMATION;
  execFileSync("/usr/bin/open", [
    "-n",
    "-a",
    release,
    "--env",
    `TWR_HOME=${home}`,
    "--env",
    `HOME=${home}`,
    "--env",
    "TWR_DESKTOP_TEST_AUTOMATION=1",
    "--env",
    "SHELL=/bin/sh",
    "--env",
    "PATH=/usr/bin:/bin",
    "--args",
    `--user-data-dir=${userData}`,
    `--remote-debugging-port=${port}`,
  ]);
  try {
    // The app is up once it has started this fixture's hub.
    let started = false;
    for (let attempt = 0; attempt < 300 && !started; attempt++) {
      try {
        execFileSync(twr, ["info", "--json"], { env: hubEnv, stdio: "pipe" });
        started = true;
      } catch {
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
    }
    assert(started, "The release app did not start its hub");
    assert(
      launcherPID(release, userData, port),
      "The release app exited instead of ignoring the automation variable",
    );
    const response = await fetch(`http://127.0.0.1:${port}/json/version`, {
      signal: AbortSignal.timeout(2000),
    }).catch((error) => error);
    assert(
      response instanceof Error,
      "The release app opened remote debugging without the QA marker",
    );
    const log = readFileSync(join(userData, "logs/twr-desktop.log"), "utf8");
    assert.match(log, /Ignored TWR_DESKTOP_TEST_AUTOMATION/);
    console.log(
      "PASS: without the QA marker the release app ignores TWR_DESKTOP_TEST_AUTOMATION and keeps remote debugging shut",
    );
  } finally {
    const pid = launcherPID(release, userData, port);
    if (pid) process.kill(pid, "SIGKILL");
    try {
      execFileSync(twr, ["hub", "stop"], {
        env: hubEnv,
        stdio: "ignore",
        timeout: 15000,
      });
    } catch {}
  }
}
// The copy under test, with the marker. It is not re-signed: the added file breaks the
// bundle's seal, which macOS checks only for a quarantined (downloaded) app.
const application = join(fixture, "twr.app");
execFileSync("/usr/bin/ditto", [release, application]);
writeFileSync(
  join(application, "Contents/Resources", QA_MARKER),
  "This copy of twr is a QA build: it honours TWR_DESKTOP_TEST_AUTOMATION. Never ship it.\n",
);
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
function fixtureLauncherPID() {
  for (const line of execFileSync("/bin/ps", ["-axo", "pid=,command="], {
    encoding: "utf8",
  }).split("\n")) {
    const match = line.trim().match(/^(\d+) (.+)$/);
    if (
      match &&
      match[2].startsWith(application + "/Contents/MacOS/twr ") &&
      match[2].includes(`--user-data-dir=${join(fixture, "user-data")}`) &&
      match[2].includes(`--remote-debugging-port=${port}`)
    )
      return Number(match[1]);
  }
  throw new Error(
    "Could not identify this fixture's app process for foreground evidence",
  );
}
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
  const context = browser.contexts()[0];
  assert(context, "Packaged application must expose its browser context");
  const page =
    context.pages()[0] ??
    (await context.waitForEvent("page", { timeout: 60000 }));
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
  // The Mac app ships no bin/digests.json (scripts/after-pack.mjs: signing changes the
  // engines). Its engines are staged in the directory named from their actual digests, and
  // the app's digest cache keeps them, keyed by the bundle's stable paths.
  {
    assert(
      !existsSync(join(application, "Contents/Resources/bin/digests.json")),
      "The Mac app must not ship engine digests that its signature invalidates",
    );
    const names = ["twr", "twr-desktop-bridge", "twr-mobile-gateway"];
    const actual = names.map((name) => [
      name,
      createHash("sha256")
        .update(readFileSync(join(application, "Contents/Resources/bin", name)))
        .digest("hex"),
    ]);
    const directory = createHash("sha256")
      .update(JSON.stringify(actual))
      .digest("hex");
    assert(
      existsSync(join(fixture, "user-data/engines", directory, "twr")),
      "The engines must be staged under their digests",
    );
    const cached = JSON.parse(
      readFileSync(join(fixture, "user-data/engines/digests.json"), "utf8"),
    );
    for (const [name, digest] of actual)
      assert.equal(
        cached[join(application, "Contents/Resources/bin", name)]?.hash,
        digest,
        `The digest cache must hold the bundle's ${name}`,
      );
    console.log(
      "PASS: the packaged app stages its engines under their digests and caches the bundle's",
    );
  }
  await page.evaluate(() =>
    window.twr.rpc("thread.new", {
      project: "demo",
      name: "packaged",
      runtime: "fake",
    }),
  );
  const attachmentPath = await page.evaluate(async () => {
    const thread = await window.twr.rpc("thread.get", { thread: "packaged" });
    const result = await window.twr.uploadFile(
      new File(["PACKAGED_ATTACHMENT_✓"], "packaged-attachment.txt", {
        type: "text/plain",
      }),
      thread.id,
    );
    return result.path;
  });
  assert(
    attachmentPath.startsWith(fixture + "/"),
    "Packaged upload must remain in its isolated fixture",
  );
  assert.equal(readFileSync(attachmentPath, "utf8"), "PACKAGED_ATTACHMENT_✓");
  console.log(
    "PASS: packaged local File upload reached the pinned worktree through production authentication",
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
  const directComposer = page.getByRole("textbox", {
    name: "Message packaged directly",
    exact: true,
  });
  const assistantMessages = await page.locator(".block-assistant").count();
  await page.getByLabel("Upload files", { exact: true }).setInputFiles({
    name: "preview.txt",
    mimeType: "text/plain",
    buffer: Buffer.from(
      "PLAIN_TEXT_PREVIEW_<script>window.unsafePreview=true</script>",
    ),
  });
  const attachment = page
    .locator(".attachment-item")
    .filter({ hasText: "preview.txt" });
  await attachment.getByText("Ready to send", { exact: false }).waitFor();
  await attachment.getByText("Preview text", { exact: false }).click();
  assert(
    (await attachment.locator("pre").innerText()).includes(
      "<script>window.unsafePreview=true</script>",
    ),
  );
  assert.equal(
    await page.evaluate(() => window.unsafePreview),
    undefined,
    "Text previews must remain inert",
  );
  assert(
    (await directComposer.inputValue()).includes("Uploaded file: preview.txt"),
  );
  assert.equal(
    await page.locator(".block-assistant").count(),
    assistantMessages,
    "Selecting a file must not send a prompt",
  );
  await attachment
    .getByRole("button", { name: "Dismiss", exact: true })
    .click();
  await directComposer.fill("");
  console.log(
    "PASS: packaged attachment preview is inert, progresses to ready, and waits for explicit send",
  );
  // Application services: settings, background mode, diagnostics and the
  // update policy of an ad hoc signed build. Launch at login is not toggled
  // here because macOS login items belong to the real user session.
  {
    const settings = await page.evaluate(() => window.twrApp.settings());
    assert.equal(settings.update.kind, "mac");
    assert.equal(
      settings.update.automatic,
      false,
      "Ad hoc builds must not self-update",
    );
    assert.match(settings.update.manualReason, /Developer ID/);
    assert.equal(settings.login.supported, true);
    assert.equal(settings.tray.available, true);
    assert.equal(settings.tray.enabled, true);
    const trayOff = await page.evaluate(() => window.twrApp.setTray(false));
    assert.equal(trayOff.tray.enabled, false);
    await page.evaluate(() => window.twrApp.setTray(true));
    await page.evaluate(() =>
      window.twrApp.setAttention({
        count: 2,
        items: [
          {
            workspace: "default",
            thread: "packaged",
            title: "packaged",
            summary: "QA",
            id: "packaged-qa",
            revision: 1,
            reason: "permission",
          },
        ],
        workspaces: [{ name: "default" }],
      }),
    );
    // Background mode: closing the window hides it and keeps the renderer live.
    await page.evaluate(() => window.twrApp.setBackground(true));
    // The automation-only SIGUSR2 hook calls BrowserWindow.close(), the path
    // of the title-bar button and File › Close. (Synthetic keystrokes do not
    // reach native menu accelerators, and a script's window.close() skips the
    // window's close event.)
    process.kill(fixtureLauncherPID(), "SIGUSR2");
    const windowState = async () =>
      JSON.parse(
        /^window: (.*)$/m.exec(
          await page.evaluate(() => window.twrApp.diagnostics()),
        )[1],
      );
    for (let i = 0; i < 50 && (await windowState()).visible; i++)
      await new Promise((resolve) => setTimeout(resolve, 100));
    assert.equal(
      (await windowState()).visible,
      false,
      "Background mode must hide the window instead of closing it",
    );
    // Launching again (a launcher, the Dock or a login item) hands over to the
    // running instance, which shows its hidden window and then exits.
    execFileSync(
      join(application, "Contents/MacOS/twr"),
      [`--user-data-dir=${join(fixture, "user-data")}`],
      {
        stdio: "ignore",
        timeout: 30000,
        env: {
          ...process.env,
          HOME: fixture,
          TWR_HOME: fixture,
          TWR_DESKTOP_TEST_AUTOMATION: "1",
        },
      },
    );
    for (let i = 0; i < 50 && !(await windowState()).visible; i++)
      await new Promise((resolve) => setTimeout(resolve, 100));
    if (!(await windowState()).visible)
      console.log(await page.evaluate(() => window.twrApp.diagnostics()));
    assert.equal((await windowState()).visible, true);
    await page
      .locator(".connection-status")
      .filter({ hasText: "Hub connected" })
      .waitFor({ timeout: 10000 });
    await page.evaluate(() => window.twrApp.setBackground(false));
    await page.evaluate(() =>
      window.twrApp.reportError({
        message: "packaged QA probe token=SHOULD_NOT_APPEAR",
        source: "qa",
      }),
    );
    const diagnostics = await page.evaluate(() => window.twrApp.diagnostics());
    assert.match(diagnostics, /^install: mac$/m);
    assert.match(diagnostics, /packaged QA probe token=\[redacted\]/);
    assert(!diagnostics.includes("SHOULD_NOT_APPEAR"));
    assert(
      !diagnostics.includes(fixture),
      "Diagnostics must hide the home path",
    );
    await page
      .getByRole("button", { name: "Settings", exact: true })
      .first()
      .click();
    await page.getByRole("heading", { name: "Application" }).waitFor();
    await page.getByRole("switch", { name: "Launch at login" }).waitFor();
    await page.getByRole("heading", { name: "Diagnostics" }).waitFor();
    console.log(
      "PASS: packaged tray settings, background close/reopen, redacted diagnostics and ad hoc update policy",
    );
  }
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
    .getByRole("button", { name: /^(Close terminal|Remove tab)$/ })
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
  if (process.env.TWR_PERF === "1") {
    const helper = join(fixture, "mac-foreground");
    execFileSync(
      "/usr/bin/xcrun",
      [
        "swiftc",
        fileURLToPath(new URL("./mac-foreground.swift", import.meta.url)),
        "-module-cache-path",
        join(fixture, "swift-modules"),
        "-o",
        helper,
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    const pid = fixtureLauncherPID();
    const sample = (activate = false) =>
      JSON.parse(
        execFileSync(
          helper,
          [String(pid), ...(activate ? ["--activate"] : [])],
          { encoding: "utf8" },
        ),
      );
    await page.bringToFront();
    const before = sample(true);
    const foreground = (state) =>
      state.targetActive &&
      state.targetOnScreen &&
      state.frontmostPID === pid &&
      state.overlappingHigherWindows === 0;
    const requireForeground = process.env.TWR_PERF_REQUIRE_FOREGROUND === "1";
    writeFileSync(
      "test-results/packaged-native-foreground.json",
      JSON.stringify({ before, after: null }, null, 2) + "\n",
    );
    if (requireForeground)
      assert(
        foreground(before),
        "Performance requires this fixture app foreground and unoccluded: " +
          JSON.stringify(before),
      );
    await performanceAudit(page, "mac-packaged");
    const after = sample();
    const classification =
      foreground(before) && foreground(after)
        ? "foreground-unoccluded"
        : "occluded-or-inactive";
    const evidence = { before, after, classification };
    writeFileSync(
      "test-results/packaged-native-foreground.json",
      JSON.stringify(evidence, null, 2) + "\n",
    );
    console.log("PERFORMANCE_CONTEXT", JSON.stringify(evidence));
    if (requireForeground)
      assert(
        foreground(after),
        "Native foreground changed during the performance audit",
      );
  }
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
