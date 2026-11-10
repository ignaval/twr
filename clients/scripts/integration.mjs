// Real Electron + real Rust hub + twr's deterministic test adapter. No provider account,
// user's config, network agent, or existing workspace is touched.
import { performanceAudit } from "./performance.mjs";
import { _electron as electron, chromium } from "@playwright/test";
import { createServer } from "node:net";
import { spawn, execFileSync } from "node:child_process";
import {
  mkdtempSync,
  realpathSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  chmodSync,
  existsSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
async function within(promise, ms, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), ms);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
const home = realpathSync(mkdtempSync(join(tmpdir(), "twr-desktop-")));
const repo = join(home, "repo");
mkdirSync(repo);
execFileSync("git", ["init", "-b", "main", repo], { stdio: "ignore" });
execFileSync(
  "git",
  [
    "-C",
    repo,
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
  join(home, "config.toml"),
  `default_runtime = "fake"\n[projects.demo]\nrepo = ${JSON.stringify(repo)}\n[orchestrator]\nautostart = false\n[threads]\nidle_compaction = "off"\n[tui]\nsuggest = false\nsummaries = false\n`,
  { mode: 0o600 },
);
const bin = resolve("../target/debug");
const codex = join(home, "test-codex");
const nativeCodex = join(home, "native-codex");
mkdirSync(nativeCodex);
writeFileSync(
  codex,
  `#!/bin/sh
if [ "$1" = "--version" ]; then echo 'codex-cli 0.162.1'; exit 0; fi
if [ "$1" = "login" ] && [ "$2" = "status" ]; then
  if [ -f "$CODEX_HOME/test-login" ]; then echo 'Logged in using ChatGPT' >&2; exit 0; fi
  echo 'Not logged in' >&2; exit 1
fi
if [ "$1" = "exec" ]; then
  echo called >> "$CODEX_HOME/test-small-model-called"
  cat > /dev/null
  echo '{"type":"item.completed","item":{"type":"agent_message","text":"Add tests for the updated behavior"}}'
  echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":8}}'
  exit 0
fi
if [ "$1" = "login" ]; then
  printf 'TEST_DEVICE_CODE_ABCD\\nConfirm test login: '
  read answer
  mkdir -p "$CODEX_HOME"
  touch "$CODEX_HOME/test-login"
  echo 'Successfully logged in: DESKTOP_LOGIN_OK'
  exit 0
fi
exit 1
`,
);
chmodSync(codex, 0o755);
const tokenFile = join(home, "test-user.token");
writeFileSync(tokenFile, randomUUID(), { mode: 0o600 });
const env = {
  ...process.env,
  TWR_HOME: home,
  TWR_TEST_USER_TOKEN_FILE: tokenFile,
  TWR_ENABLE_FAKE: "1",
  TWR_DESKTOP_TEST_AUTOMATION: "1",
  TWR_FAKE_ATTACH:
    'printf "ATTACHED_AGENT_OK\\n"; read reply; printf "ATTACH_INPUT:%s\\n" "$reply"',
  TWR_DESKTOP_BINARY_DIR: bin,
  TWR_DESKTOP_TEST_USER_DATA: join(home, "electron"),
  TWR_WORKSPACE: "default",
};
env.TWR_CODEX_BIN = codex;
env.TWR_CLAUDE_BIN = join(home, "not-installed-claude");
env.CODEX_HOME = nativeCodex;
env.HOME = home;
env.SHELL = "/bin/sh";
env.ENV = join(home, "shellrc");
env.PS1 = "DESKTOP_TEST> ";
env.XDG_CONFIG_HOME = join(home, "config");
env.XDG_CACHE_HOME = join(home, "cache");
writeFileSync(env.ENV, "PS1='DESKTOP_TEST> '\n");
// Prevent Electron from taking Node-only launch flags inherited from an editor.
delete env.ELECTRON_RUN_AS_NODE;
let application;
let packagedProcess;
let applicationProcess;
let packagedLog = "";
let hub;
let sideWorkspace = false;
const errors = [];
try {
  hub = spawn(join(bin, "twr"), ["hub"], {
    env,
    stdio: ["ignore", "ignore", "pipe"],
  });
  let hubErrors = "";
  hub.stderr.on("data", (data) => (hubErrors += data));
  for (let i = 0; i < 100; i++) {
    try {
      execFileSync(join(bin, "twr"), ["info", "--json"], {
        env,
        stdio: "pipe",
      });
      break;
    } catch {
      if (i === 99) throw new Error(hubErrors);
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  if (process.env.TWR_DESKTOP_EXECUTABLE) {
    const port = await new Promise((resolve, reject) => {
      const server = createServer();
      server.on("error", reject);
      server.listen(0, "127.0.0.1", () => {
        const port = server.address().port;
        server.close(() => resolve(port));
      });
    });
    packagedProcess = spawn(
      process.env.TWR_DESKTOP_EXECUTABLE,
      [
        `--remote-debugging-port=${port}`,
        `--user-data-dir=${env.TWR_DESKTOP_TEST_USER_DATA}`,
        ...(process.env.TWR_DESKTOP_WAYLAND_TEST
          ? ["--ozone-platform=wayland"]
          : []),
      ],
      { cwd: resolve("."), env, stdio: ["ignore", "pipe", "pipe"] },
    );
    packagedProcess.stdout.on("data", (chunk) => (packagedLog += chunk));
    packagedProcess.stderr.on("data", (chunk) => (packagedLog += chunk));
    packagedProcess.on("error", (error) => (packagedLog += String(error)));
    let browser;
    for (let attempt = 0; attempt < 120; attempt++) {
      try {
        browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`, {
          timeout: 500,
        });
        break;
      } catch {
        if (packagedProcess.exitCode !== null) break;
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
    }
    assert(
      browser,
      "Packaged app must expose its test Chromium port (a packaged build honours TWR_DESKTOP_TEST_AUTOMATION only with resources/twr-qa-automation in its install; docs/desktop-qa.md): " +
        packagedLog,
    );
    const context = browser.contexts()[0];
    assert(context, "Packaged application must expose its browser context");
    const page =
      context.pages()[0] ??
      (await context.waitForEvent("page", { timeout: 60000 }));
    application = {
      firstWindow: async () => page,
      close: async () => {
        await page.evaluate(() => window.twr.quit()).catch(() => {});
        await browser.close();
        const deadline = Date.now() + 10000;
        while (packagedProcess.exitCode === null && Date.now() < deadline)
          await new Promise((resolve) => setTimeout(resolve, 100));
        assert(
          packagedProcess.exitCode !== null,
          "Packaged app must quit through its normal IPC",
        );
      },
    };
  } else
    application = await electron.launch({
      args: ["."],
      cwd: resolve("."),
      env,
      timeout: 60000,
    });
  applicationProcess = packagedProcess || application.process();
  const page = await application.firstWindow();
  page.on("pageerror", (error) => errors.push(error.message));
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 30000 });
  console.log("Connected to real hub");
  if (process.env.TWR_HYPRLAND_QA === "1") {
    const clients = JSON.parse(
      execFileSync("hyprctl", ["clients", "-j"], { encoding: "utf8" }),
    );
    const client = clients.find((value) => value.class === "tools.twr.desktop");
    assert(client, "Hyprland window class must be tools.twr.desktop");
    assert.equal(client.xwayland, false);
    const monitors = JSON.parse(
      execFileSync("hyprctl", ["monitors", "-j"], { encoding: "utf8" }),
    );
    mkdirSync("test-results", { recursive: true });
    writeFileSync(
      "test-results/hyprland-window.json",
      JSON.stringify(
        {
          class: client.class,
          xwayland: client.xwayland,
          size: client.size,
          scale: monitors.find((m) => m.id === client.monitor)?.scale,
        },
        null,
        2,
      ) + "\n",
    );
    console.log(
      "PASS: native Hyprland window association and fractional-scale session",
    );
  }

  await page
    .getByRole("button", { name: "Choose your orchestrator" })
    .waitFor();
  const catalog = await page.evaluate(() => window.twr.catalog());
  console.log("Discovered CLI catalog:", catalog.length);
  assert(
    catalog.length > 60,
    `Expected full CLI catalog; found ${catalog.length}`,
  );
  for (const command of [
    "account add",
    "account login",
    "box add",
    "box project",
    "box doctor",
    "remote",
    "runner install-token",
    "terminal-setup",
    "log checkpoint",
    "events",
  ])
    assert(
      catalog.some((c) => c.cli.join(" ") === command),
      `Missing CLI workflow: ${command}`,
    );
  assert(
    catalog
      .find((c) => c.cli.join(" ") === "box add")
      .fields.some((f) => f.key === "name"),
    "box add next-line help must produce fields",
  );
  const uploadPath = await page.evaluate(async () => {
    const thread = await window.twr.rpc("thread.new", {
      project: "demo",
      name: "api",
      runtime: "fake",
    });
    const uploaded = await window.twr.uploadFile(
      new File(["DESKTOP_ATTACHMENT_UTF8_✓"], "desktop-attachment.txt", {
        type: "text/plain",
      }),
      thread.id,
    );
    await window.twr.rpc("thread.send", {
      thread: thread.id,
      prompt: "Hello desktop\n!perm Bash printf safe\nFinished safely",
    });
    return uploaded.path;
  });
  assert.equal(readFileSync(uploadPath, "utf8"), "DESKTOP_ATTACHMENT_UTF8_✓");
  console.log(
    "Local attachment reached its worktree through the actual companion",
  );
  await page
    .getByRole("button", { name: /api.*waiting/ })
    .first()
    .waitFor({ timeout: 30000 });
  // An answer counts only once the options have been on screen for 600 ms, as in the
  // TUI (shared/arming.ts): a click the moment the card appears answers nothing.
  await page.evaluate(() => {
    window.__earlyClick = new Promise((resolve) => {
      const tryClick = () => {
        const card = [...document.querySelectorAll(".manager .attention-card")]
          .filter((node) => /Permission requested/.test(node.textContent || ""))
          .at(0);
        const options = card?.querySelector(".attention-options");
        const allow = [
          ...(card?.querySelectorAll(".attention-options button") || []),
        ].find((button) => /allow/i.test(button.textContent || ""));
        if (!allow) return false;
        const armed = options?.getAttribute("data-armed") === "true";
        allow.click();
        observer.disconnect();
        resolve(!armed);
        return true;
      };
      const observer = new MutationObserver(tryClick);
      observer.observe(document.body, { childList: true, subtree: true });
      tryClick();
    });
  });
  await page.getByRole("button", { name: /Needs you/ }).click();
  await page.getByText("Permission requested", { exact: true }).waitFor();
  const earlyClick = await within(
    page.evaluate(() => window.__earlyClick),
    10000,
    "The permission card never showed an Allow button",
  );
  assert(earlyClick, "The early click must land before the options arm");
  console.log("Permission request rendered");
  const item = await page.evaluate(async () =>
    (await window.twr.rpc("attention.list", { all: true })).find(
      (a) => a.reason === "permission",
    ),
  );
  assert(item.revision > 0);
  await assert.rejects(() =>
    page.evaluate(
      (item) =>
        window.twr.rpc("attention.respond", {
          attn_id: item.attn_id,
          revision: item.revision - 1,
          answer: "allow",
        }),
      item,
    ),
  );
  const permissionCard = page
    .locator(".manager .attention-card")
    .filter({ hasText: "Permission requested" })
    .first();
  await new Promise((resolve) => setTimeout(resolve, 300));
  const still = await page.evaluate(
    async (item) =>
      (await window.twr.rpc("attention.list", { all: true })).some(
        (a) => a.attn_id === item.attn_id && a.revision === item.revision,
      ),
    item,
  );
  assert(still, "A click before the options armed must answer nothing");
  assert.equal(await permissionCard.locator(".attention-sent").count(), 0);
  console.log("PASS: a click the moment a request appears answers nothing");
  await permissionCard
    .locator('.attention-options[data-armed="true"]')
    .waitFor({ timeout: 5000 });
  await permissionCard
    .locator(".attention-options button")
    .filter({ hasText: "allow" })
    .first()
    .click();
  await page
    .locator(".manager .attention-kind")
    .filter({ hasText: "Permission requested" })
    .waitFor({ state: "hidden", timeout: 15000 });
  console.log("Revision-bound permission answered");
  // Conversation and message routing: selecting a card doesn't change the destination.
  await page
    .getByRole("button", { name: /api.*done/ })
    .first()
    .click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await page.getByRole("textbox", { name: "Message api directly" }).waitFor();
  await page.getByText("Hello desktop", { exact: true }).waitFor();
  await page
    .locator(".conversation-scroll")
    .getByText("Finished safely", { exact: true })
    .waitFor();
  const fullViewport = await page.evaluate(() => ({
    width: innerWidth,
    height: innerHeight,
  }));
  await page.setViewportSize({ width: 420, height: 480 });
  await page.evaluate(() =>
    window.twr.rpc("thread.send", {
      thread: "api",
      prompt:
        "!perm Bash UI_LAYOUT_BOUNDARY " +
        "Review this command carefully. ".repeat(300),
    }),
  );
  const longRequest = page
    .locator(".conversation-attention .attention-card")
    .filter({ hasText: "UI_LAYOUT_BOUNDARY" });
  await longRequest.waitFor({ timeout: 15000 });
  const layout = await page.evaluate(() => {
    const requests = document.querySelector(".conversation-attention");
    return {
      bottom: document
        .querySelector('button[aria-label="Send message"]')
        .getBoundingClientRect().bottom,
      height: innerHeight,
      scrollable: requests.scrollHeight > requests.clientHeight,
    };
  });
  assert(
    layout.scrollable,
    "Long inline requests must scroll within their own region",
  );
  assert(
    layout.bottom <= layout.height + 1,
    "Inline requests must leave the composer visible",
  );
  const deny = longRequest
    .locator(".attention-options button")
    .filter({ hasText: "deny" });
  await deny.scrollIntoViewIfNeeded();
  assert(
    await deny.isVisible(),
    "The full request and Deny action must share one reachable scroll area",
  );
  // Scrolled into view just now: its options arm 600 ms after they came on screen.
  await longRequest
    .locator('.attention-options[data-armed="true"]')
    .waitFor({ timeout: 5000 });
  await deny.click();
  await longRequest.waitFor({ state: "hidden" });
  await page.setViewportSize(fullViewport);
  const showSidebar = page.getByRole("button", {
    name: "Show sidebar",
    exact: true,
  });
  if (await showSidebar.isVisible()) await showSidebar.click();
  console.log(
    "PASS: long inline requests preserve a visible composer in narrow windows",
  );
  await page
    .getByRole("textbox", { name: "Message api directly" })
    .fill("Second desktop turn");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  await page.getByText("Second desktop turn", { exact: true }).last().waitFor();
  // A freshly created thread has no turn yet: subscribe at zero and stream its first reply.
  await page
    .getByRole("button", { name: "New thread", exact: true })
    .first()
    .click();
  const create = page.getByRole("dialog", { name: "New thread", exact: true });
  await create.getByLabel(/Project or repository/).fill("demo");
  await create.getByLabel("Name", { exact: true }).fill("ui-first");
  await create.getByLabel("Runtime", { exact: true }).fill("fake");
  await create.locator('button[type="submit"]').click();
  const firstComposer = page.getByRole("textbox", {
    name: "Message ui-first directly",
  });
  await firstComposer.fill("First turn from the graphical client");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  await page
    .locator(".conversation-scroll .block-assistant")
    .getByText("First turn from the graphical client", { exact: true })
    .waitFor();
  await page
    .locator(".thread-list .thread-card")
    .filter({ hasText: "api" })
    .click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  // Structured forms really run in a PTY, and interactive attach reaches the same session.
  const session = await page.evaluate(() =>
    window.twr.terminalOpen({ args: ["attach", "api"], title: "Attach test" }),
  );
  let terminalText = "";
  for (let i = 0; i < 100; i++) {
    terminalText = (
      await page.evaluate((id) => window.twr.terminalRead(id), session.id)
    ).data;
    if (terminalText.includes("ATTACHED_AGENT_OK")) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  assert(terminalText.includes("ATTACHED_AGENT_OK"), terminalText);
  await page.evaluate(
    (id) => window.twr.terminalWrite(id, "desktop-input\r"),
    session.id,
  );
  for (let i = 0; i < 100; i++) {
    terminalText = (
      await page.evaluate((id) => window.twr.terminalRead(id), session.id)
    ).data;
    if (terminalText.includes("ATTACH_INPUT:desktop-input")) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  assert(terminalText.includes("ATTACH_INPUT:desktop-input"), terminalText);
  await page.evaluate((id) => window.twr.terminalClose(id), session.id);
  // The actual xterm UI accepts keys, preserves terminal editing shortcuts, and survives reload.
  await page.getByRole("button", { name: "Open shell", exact: true }).click();
  const surface = page.locator(".terminal-session:visible");
  await surface.waitFor();
  const shellId = await surface.getAttribute("data-session-id");
  for (let i = 0; i < 100; i++) {
    const output = await page.evaluate(
      (id) => window.twr.terminalRead(id),
      shellId,
    );
    if (output.data.includes("DESKTOP_TEST>")) break;
    if (i === 99) throw new Error("Test shell prompt did not become ready");
    await new Promise((r) => setTimeout(r, 50));
  }
  const keyboard = surface.locator(".xterm-helper-textarea");
  await keyboard.focus();
  await keyboard.press("Control+k");
  assert.equal(
    await page.getByRole("dialog").count(),
    0,
    "Ctrl-K must remain a terminal editing key",
  );
  // dash may retain Ctrl-K literally; clear the line with the POSIX kill key.
  await keyboard.press("Control+u");
  await keyboard.pressSequentially("printf 'ELECTRON_TERMINAL_UI_OK\\n'", {
    delay: 1,
  });
  await keyboard.press("Enter");
  for (let i = 0; i < 100; i++) {
    const output = await page.evaluate(
      (id) => window.twr.terminalRead(id),
      shellId,
    );
    if (output.data.includes("ELECTRON_TERMINAL_UI_OK\r\n")) break;
    if (i === 99)
      throw new Error("xterm keyboard input did not reach the shell");
    await new Promise((r) => setTimeout(r, 100));
  }
  await page.reload();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .locator(`.terminal-session[data-session-id="${shellId}"]`)
    .waitFor();
  assert(
    (
      await page.evaluate((id) => window.twr.terminalRead(id), shellId)
    ).data.includes("ELECTRON_TERMINAL_UI_OK"),
  );
  await page.getByRole("button", { name: "Close Shell", exact: true }).click();
  await page
    .getByRole("button", { name: /^(Close terminal|Remove tab)$/ })
    .click();
  // Account login uses a disposable provider stub, not a real account or credential.
  execFileSync(
    join(bin, "twr"),
    [
      "account",
      "add",
      "codex",
      "work",
      "--share",
      "none",
      "--no-login",
      "--native-name",
      "personal",
    ],
    { env, stdio: "pipe" },
  );
  await page
    .getByRole("button", { name: "Open command palette", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "Search commands" })
    .fill("twr account login");
  await page.getByText("twr account login", { exact: true }).click();
  await page.locator("#field-runtime").fill("codex");
  await page.locator("#field-name").fill("work");
  await page
    .getByRole("button", { name: "Open terminal", exact: true })
    .click();
  const loginSurface = page.locator(".terminal-session:visible");
  await loginSurface.waitFor();
  const loginId = await loginSurface.getAttribute("data-session-id");
  for (let i = 0; i < 100; i++) {
    const output = await page.evaluate(
      (id) => window.twr.terminalRead(id),
      loginId,
    );
    if (output.data.includes("TEST_DEVICE_CODE_ABCD")) break;
    if (i === 99) throw new Error("Interactive account login did not start");
    await new Promise((r) => setTimeout(r, 100));
  }
  await loginSurface
    .locator(".xterm-helper-textarea")
    .pressSequentially("confirm");
  await loginSurface.locator(".xterm-helper-textarea").press("Enter");
  for (let i = 0; i < 100; i++) {
    const output = await page.evaluate(
      (id) => window.twr.terminalRead(id),
      loginId,
    );
    if (output.data.includes("DESKTOP_LOGIN_OK")) break;
    if (i === 99)
      throw new Error("Interactive login did not receive keyboard input");
    await new Promise((r) => setTimeout(r, 100));
  }
  await page
    .getByRole("button", { name: "Close twr account login", exact: true })
    .click();
  await page
    .getByRole("button", { name: /^(Close terminal|Remove tab)$/ })
    .click();
  console.log(
    "Embedded terminal UI, shortcut handling, reload restore and interactive account login passed",
  );
  // Small-model assistance must show its disclosure before even the disposable provider runs.
  writeFileSync(join(nativeCodex, "test-login"), "fixture");
  await page.evaluate(() => window.twr.configSet(["tui"], "suggest", true));
  await page.reload();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .locator(".thread-list .thread-card")
    .filter({ hasText: "api" })
    .click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await page
    .getByRole("button", {
      name: "Continue with configured features",
      exact: true,
    })
    .waitFor();
  assert.equal(
    existsSync(join(nativeCodex, "test-small-model-called")),
    false,
    "No provider call before disclosure",
  );
  await page
    .getByRole("button", {
      name: "Continue with configured features",
      exact: true,
    })
    .click();
  const suggestedComposer = page.getByRole("textbox", {
    name: "Message api directly",
  });
  // The suggestion is the empty box's placeholder, as the TUI's ghost text.
  await page
    .locator('textarea[placeholder="Add tests for the updated behavior"]')
    .waitFor({ timeout: 30000 })
    .catch(async (error) => {
      const diagnostic = await page.evaluate(async () => {
        const config = await window.twr.configRead();
        const transcript = await window.twr.rpc("thread.transcript", {
          thread: "api",
          since_seq: 0,
          limit: 500,
        });
        const turn = transcript.events.findLast(
          (event) => event.kind === "turn.ended",
        )?.turn;
        return {
          turn,
          answer: await window.twr.rpc("suggest.next", {
            thread: "api",
            turn,
            models: config.settings.tui.suggest_model,
            retry: true,
          }),
          agents: (await window.twr.rpc("hub.info")).agents,
        };
      });
      console.error("Suggestion diagnostics:", JSON.stringify(diagnostic));
      throw error;
    });
  await page.getByText("takes the suggestion").waitFor();
  assert.equal(
    await page.locator(".suggested-message").count(),
    0,
    "Desktop shows the suggestion in the box, not beside it",
  );
  await suggestedComposer.focus();
  // Typing types over it; with the box empty again it shows again.
  await suggestedComposer.press("x");
  assert.equal(await suggestedComposer.inputValue(), "x");
  await suggestedComposer.press("Backspace");
  await page
    .locator('textarea[placeholder="Add tests for the updated behavior"]')
    .waitFor({ timeout: 30000 });
  // Tab or → takes it once it has been on screen a moment (300 ms, as the TUI).
  await page.waitForTimeout(400);
  await suggestedComposer.press("ArrowRight");
  assert.equal(
    await suggestedComposer.inputValue(),
    "Add tests for the updated behavior",
  );
  await suggestedComposer.fill("");
  await page
    .locator('textarea[placeholder="Add tests for the updated behavior"]')
    .waitFor({ timeout: 30000 });
  // Tab or → takes it once it has been on screen a moment (300 ms, as the TUI).
  await page.waitForTimeout(400);
  await suggestedComposer.press("Tab");
  assert.equal(
    await suggestedComposer.inputValue(),
    "Add tests for the updated behavior",
  );
  assert.equal(
    await page
      .locator(".conversation-scroll")
      .getByText("Add tests for the updated behavior", { exact: true })
      .count(),
    0,
    "Accepting never sends a suggestion",
  );
  await suggestedComposer.fill("");
  await page
    .getByRole("button", { name: "Settings", exact: true })
    .first()
    .click();
  const suggestionSetting = page.getByRole("checkbox", {
    name: "Suggest next messages",
    exact: true,
  });
  await suggestionSetting.uncheck();
  const suggestionRow = page
    .locator(".setting-row")
    .filter({ has: suggestionSetting });
  await suggestionRow
    .getByRole("button", { name: "Save", exact: true })
    .click();
  await suggestionRow
    .getByRole("button", { name: "Saved", exact: true })
    .waitFor();
  await page
    .locator(".thread-list .thread-card")
    .filter({ hasText: "api" })
    .click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await suggestedComposer.waitFor();
  await page.waitForTimeout(1000);
  assert.notEqual(
    await suggestedComposer.getAttribute("placeholder"),
    "Add tests for the updated behavior",
    "Settings changes apply when returning to chat without reloading",
  );
  console.log(
    "Small-model disclosure, provider stub, placeholder suggestion, Tab and → acceptance and explicit send boundary passed",
  );
  // Real configuration CAS and validation, with an external edit between read and save.
  const config = await page.evaluate(() => window.twr.configRead());
  await page.evaluate(async (config) => {
    await window.twr.configSave(
      config.content + "\n# desktop edit\n",
      config.hash,
    );
  }, config);
  await assert.rejects(() =>
    page.evaluate(
      (config) => window.twr.configSave(config.content, config.hash),
      config,
    ),
  );
  assert(
    readFileSync(join(home, "config.toml"), "utf8").includes("# desktop edit"),
  );
  await page.evaluate(async () => {
    await window.twr.rpc("memory.save", {
      name: "desktop-note",
      type: "project",
      description: "Desktop integration note",
      body: "Preserve user authority.",
    });
  });
  const note = await page.evaluate(() =>
    window.twr.rpc("memory.read", { name: "desktop-note" }),
  );
  assert.equal(note.body, "Preserve user authority.");
  // Launch boundary rejects machine identities and protocol-handler execution.
  await assert.rejects(() =>
    page.evaluate(() => window.twr.rpc("hello", { client: "orchestrator" })),
  );
  await assert.rejects(() =>
    page.evaluate(() => window.twr.openExternal("file:///etc/passwd")),
  );
  await assert.rejects(() =>
    page.evaluate(() =>
      window.twr.cli(["thread", "list", "--workspace", "other"]),
    ),
  );
  // Narrow tiled window: navigation, composer and dialogs remain reachable.
  await page
    .locator(".thread-list .thread-card")
    .filter({ hasText: "api" })
    .click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await page.setViewportSize({ width: 520, height: 850 });
  assert(
    await page
      .getByRole("textbox", { name: "Message api directly" })
      .isVisible(),
  );
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth > window.innerWidth,
  );
  assert.equal(overflow, false);
  await page
    .locator(".compact-nav")
    .getByRole("button", { name: "More" })
    .click();
  await page
    .locator(".manage-nav")
    .evaluate((element) => (element.open = true));
  await page
    .locator(".sidebar-secondary")
    .getByRole("button", { name: "Accounts", exact: true })
    .click();
  await page.getByRole("heading", { name: "Accounts", exact: true }).waitFor();
  await page
    .locator(".compact-nav")
    .getByRole("button", { name: "Chat", exact: true })
    .click();
  await page.setViewportSize({ width: 420, height: 480 });
  assert(
    await page
      .getByRole("textbox", { name: "Message api directly" })
      .isVisible(),
  );
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth > window.innerWidth,
    ),
    false,
  );
  const validConfig = readFileSync(join(home, "config.toml"), "utf8");
  writeFileSync(join(home, "config.toml"), "[invalid config");
  await page
    .evaluate(() => window.twr.connect("broken-cli-qa"))
    .catch(() => {});
  writeFileSync(join(home, "config.toml"), validConfig);
  const blocked = await page.evaluate(async () => {
    try {
      await window.twr.cli(["ws", "new", "wrong-destination"]);
      return "unexpected success";
    } catch (error) {
      return String(error);
    }
  });
  assert.match(blocked, /selected workspace/);
  assert(!existsSync(join(home, "data", "workspaces", "wrong-destination")));
  await page.evaluate(() => window.twr.connect("default"));
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ state: "attached" });
  console.log(
    "PASS: a failed workspace connection cannot run CLI mutations in the previous workspace",
  );
  const dismissError = page.getByRole("button", {
    name: "Dismiss error",
    exact: true,
  });
  if (await dismissError.isVisible()) await dismissError.click();
  // Every workspace in the tray and badge, and OS notifications while the window is
  // hidden. Automation records what would be posted (desktop/app-services.ts testPost)
  // instead of relying on the system, which may not let a development build notify.
  if (!packagedProcess) {
    await page.setViewportSize(fullViewport);
    const twr = join(bin, "twr");
    const side = (...args) =>
      execFileSync(twr, ["-w", "side", ...args], {
        env,
        encoding: "utf8",
        stdio: "pipe",
      });
    execFileSync(twr, ["ws", "new", "side"], { env, stdio: "pipe" });
    sideWorkspace = true;
    // The app starts this workspace's hub itself. That hub must keep nothing of the app's:
    // on macOS it once inherited the crash reporter's exception ports, which kept
    // Electron's crash handler, and with it this runner's pipes, open past the app's exit,
    // so that closing the app below never finished.
    const send = (event) =>
      application.evaluate(
        ({ BrowserWindow }, event) =>
          BrowserWindow.getAllWindows()[0].webContents.send("twr:event", event),
        event,
      );
    const showing = async (name) => {
      await page
        .locator(".workspace-breadcrumb")
        .filter({ hasText: new RegExp(`^${name}$`) })
        .waitFor({ state: "attached", timeout: 30000 });
      await page
        .locator(".connection-status")
        .filter({ hasText: "Hub connected" })
        .waitFor({ state: "attached", timeout: 30000 });
    };
    // Visiting the new workspace once starts its hub and lists it in the window.
    await send({ type: "menu-attention", workspace: "side" });
    await showing("side");
    side("thread", "new", "demo", "--runtime", "fake", "--name", "side-api");
    await send({ type: "menu-attention", workspace: "default" });
    await showing("default");
    const recorded = () =>
      application.evaluate(() => {
        const state = globalThis.__twrTest;
        return {
          attention: state.attention,
          notifications: state.notifications.map(
            ({ title, body, target, closed }) => ({
              title,
              body,
              target,
              closed,
            }),
          ),
        };
      });
    const until = async (check, message) => {
      const deadline = Date.now() + 30000;
      for (;;) {
        // The main process's beat for reading the other workspaces, sooner.
        await send({ type: "inbox-poll" });
        const value = await recorded();
        if (check(value)) return value;
        if (Date.now() > deadline)
          throw new Error(`${message}: ${JSON.stringify(value)}`);
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
    };
    // Background mode hides the window; its badge and notifications stay live.
    await application.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].hide(),
    );
    await until(
      (state) =>
        state.attention?.workspaces.some((w) => w.name === "side" && w.stream),
      "The side workspace was never read",
    );
    const before = (await recorded()).notifications.length;
    side("send", "side-api", "!perm Bash printf SIDE_SECRET_COMMAND");
    let state = await until(
      (state) =>
        state.notifications.length > before &&
        state.attention.items.some((i) => i.workspace === "side"),
      "No notification for a permission in the other workspace",
    );
    const notice = state.notifications.at(-1);
    assert.equal(notice.title, "side-api · side");
    assert.equal(notice.body, "Asks for permission");
    assert.equal(notice.target.workspace, "side");
    assert.equal(state.notifications.length, before + 1);
    assert(
      !JSON.stringify(state.notifications).includes("SIDE_SECRET_COMMAND"),
      "A notification must not carry what is asked",
    );
    const sideItem = state.attention.items.find((i) => i.workspace === "side");
    assert.equal(sideItem.reason, "permission");
    assert.equal(state.attention.count, state.attention.items.length);
    assert(state.attention.count >= 1);
    if (process.platform === "darwin")
      assert.equal(
        await application.evaluate(({ app }) => app.getBadgeCount()),
        state.attention.count,
        "The Dock badge counts every workspace",
      );
    side("respond", "side-api", "allow", "--yes");
    await until(
      (state) => state.notifications.at(-1).closed,
      "The notification stayed after its request was answered",
    );
    // A click shows the window in that workspace at the request; it answers nothing.
    side("send", "side-api", "!perm Bash printf SIDE_SECOND");
    state = await until(
      (state) => state.notifications.length > before + 1,
      "No notification for the second permission",
    );
    await application.evaluate(() =>
      globalThis.__twrTest.notifications.at(-1).click(),
    );
    await showing("side");
    assert(
      await application.evaluate(({ BrowserWindow }) =>
        BrowserWindow.getAllWindows()[0].isVisible(),
      ),
    );
    await page
      .locator(".manager .attention-card")
      .filter({ hasText: "Permission requested" })
      .filter({ hasText: "side-api" })
      .first()
      .waitFor({ timeout: 15000 });
    assert(
      JSON.parse(side("attention", "--json")).some(
        (item) => item.reason === "permission",
      ),
      "Opening a notification must not answer its request",
    );
    side("respond", "side-api", "allow", "--yes");
    await send({ type: "menu-attention", workspace: "default" });
    await showing("default");
    console.log(
      "PASS: tray and badge count every workspace; a hidden window notifies of another workspace's permission without its text, removes it once answered, and a click opens it there",
    );
  }
  if (!packagedProcess) {
    // Help › Third-Party Licences; automation records the file instead of opening it.
    const opened = await application.evaluate(({ Menu }) => {
      const help = Menu.getApplicationMenu().items.find(
        (item) => item.label === "Help",
      );
      help.submenu.items
        .find((item) => item.label === "Third-Party Licences")
        .click();
      return globalThis.__twrOpenedNotices;
    });
    assert.equal(
      opened?.path,
      resolve("dist-notices/THIRD-PARTY-LICENSES.html"),
    );
    console.log(
      `PASS: Help › Third-Party Licences opens the generated notices${opened.exists ? "" : " (not generated in this checkout: node scripts/notices.mjs)"}`,
    );
  }
  if (errors.length) throw new Error(errors.join("\n"));
  if (process.env.TWR_PERF === "1")
    await performanceAudit(
      page,
      process.env.TWR_PERF_LABEL || process.platform,
      packagedProcess ? undefined : application,
    );
  // Account-login and other interactive fixtures can retain a terminal even
  // after its output assertion. Close our sessions before requesting a normal
  // quit, so a native terminal-confirmation sheet cannot stall this test.
  const remainingTerminals = await page.evaluate(async () => {
    const { terminals } = await window.twr.bootstrap();
    for (const terminal of terminals)
      await window.twr.terminalClose(terminal.id);
    return terminals.map((terminal) => terminal.id);
  });
  console.log(
    "Closing integration app; fixture terminals:",
    remainingTerminals,
  );
  await within(
    application.close(),
    15000,
    "Desktop application did not quit normally after its test terminals closed",
  );
  application = undefined;
  // Closing Electron leaves the hub serving the same workspace.
  const info = JSON.parse(
    execFileSync(join(bin, "twr"), ["info", "--json"], {
      env,
      encoding: "utf8",
    }),
  );
  assert.equal(info.workspace, "default");
  console.log(
    `PASS: ${catalog.length} CLI workflows, real approvals, transcript streaming, embedded attach + input, config CAS, memory, IPC boundaries, narrow layout, independent hub lifecycle.`,
  );
} catch (error) {
  console.error("Failed:", error);
  if (application) {
    const page = await application.firstWindow();
    console.error("Renderer errors:", errors);
    console.error(
      "Window text:",
      await page
        .locator("body")
        .innerText()
        .catch(() => "<unavailable>"),
    );
    mkdirSync("test-results", { recursive: true });
    await page
      .screenshot({ path: "test-results/integration-failure.png" })
      .catch(() => {});
  }
  throw error;
} finally {
  // Failed tests may leave a native confirmation open. Force only this test's process
  // to exit so the runner and its temporary hub always reach cleanup.
  if (application) {
    if (!packagedProcess)
      await within(
        application.evaluate(({ app }) => app.exit(0)),
        3000,
        "Test application did not acknowledge forced cleanup",
      ).catch(() => {});
  }
  const fixtureProcess = applicationProcess || packagedProcess;
  if (fixtureProcess?.exitCode === null && fixtureProcess.signalCode === null)
    fixtureProcess.kill("SIGKILL");
  if (application) {
    await within(
      application.close(),
      3000,
      "Test application cleanup timed out",
    ).catch(() => {});
  }
  if (sideWorkspace)
    try {
      execFileSync(join(bin, "twr"), ["-w", "side", "hub", "stop"], {
        env,
        stdio: "ignore",
        timeout: 15000,
      });
    } catch {}
  try {
    execFileSync(join(bin, "twr"), ["hub", "stop"], {
      env,
      stdio: "ignore",
      timeout: 15000,
    });
  } catch {
    hub?.kill("SIGTERM");
  }
  if (hub && hub.exitCode === null && hub.signalCode === null)
    await Promise.race([
      new Promise((resolve) => hub.once("exit", resolve)),
      new Promise((resolve) => setTimeout(resolve, 5000)),
    ]);
  rmSync(home, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}
