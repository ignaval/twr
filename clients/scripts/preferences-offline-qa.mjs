// Focused desktop layout, encrypted recovery, and outbox QA. All hub processes and SQLite rows are disposable.
import { clientProtocol } from "./client-protocol.mjs";
import { stopFixture } from "./fixture-cleanup.mjs";
import { _electron as electron, expect } from "@playwright/test";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  readdirSync,
  statSync,
  rmSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { createConnection, createServer } from "node:net";
import { createHash, randomUUID } from "node:crypto";
const home = mkdtempSync(join("/tmp", "twr-preferences-offline-"));
const bin = resolve("../target/debug");
const repo = join(home, "repo");
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
    "Fixture",
  ],
  { stdio: "ignore" },
);
writeFileSync(
  join(home, "config.toml"),
  `default_runtime="fake"\n[orchestrator]\nautostart=false\n[threads]\nidle_compaction="off"\n[tui]\nsuggest=false\nsummaries=false\n`,
  { mode: 0o600 },
);
const token = randomUUID();
const tokenFile = join(home, "user.token");
writeFileSync(tokenFile, token, { mode: 0o600 });
const env = {
  ...process.env,
  TWR_HOME: home,
  TWR_TEST_USER_TOKEN_FILE: tokenFile,
  TWR_ENABLE_FAKE: "1",
  TWR_DESKTOP_TEST_AUTOMATION: "1",
  TWR_DESKTOP_BINARY_DIR: bin,
  TWR_DESKTOP_TEST_USER_DATA: join(home, "electron"),
  TWR_WORKSPACE: "default",
  HOME: home,
  XDG_CONFIG_HOME: join(home, "config"),
  XDG_CACHE_HOME: join(home, "cache"),
  SHELL: "/bin/sh",
};
const codex = join(home, "stub-codex");
writeFileSync(
  codex,
  '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "codex-cli 0.162.1"; exit 0; fi\nexit 1\n',
  { mode: 0o755 },
);
env.TWR_CODEX_BIN = codex;
env.CODEX_HOME = join(home, "codex-home");
mkdirSync(env.CODEX_HOME);
delete env.ELECTRON_RUN_AS_NODE;
const processes = [];
let app;
const errors = [];
function socket(workspace) {
  return workspace === "default"
    ? join(home, "hub.sock")
    : join(home, "ws", workspace + ".sock");
}
function rpc(workspace, method, params = {}) {
  return new Promise((resolve, reject) => {
    const connection = createConnection(socket(workspace));
    let buffer = "";
    const timeout = setTimeout(() => {
      connection.destroy();
      reject(new Error(method + " timed out"));
    }, 10000);
    connection.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
    connection.once("connect", () =>
      connection.write(
        JSON.stringify({
          id: 1,
          method: "hello",
          params: {
            client: "inbox-qa",
            protocol: clientProtocol,
            user_token: token,
          },
        }) + "\n",
      ),
    );
    connection.on("data", (chunk) => {
      buffer += chunk.toString();
      for (;;) {
        const end = buffer.indexOf("\n");
        if (end < 0) break;
        const result = JSON.parse(buffer.slice(0, end));
        buffer = buffer.slice(end + 1);
        if (result.error) {
          clearTimeout(timeout);
          connection.destroy();
          reject(new Error(JSON.stringify(result.error)));
          return;
        }
        if (result.id === 1)
          connection.write(JSON.stringify({ id: 2, method, params }) + "\n");
        if (result.id === 2) {
          clearTimeout(timeout);
          connection.end();
          resolve(result.result);
        }
      }
    });
  });
}
async function start(workspace) {
  const child = spawn(join(bin, "twr"), ["-w", workspace, "hub"], {
    env,
    stdio: ["ignore", "ignore", "pipe"],
  });
  processes.push(child);
  let startupLog = "";
  child.stderr.on("data", (chunk) => {
    startupLog += chunk.toString();
  });
  try {
    await expect
      .poll(
        () =>
          rpc(workspace, "hub.info")
            .then(() => true)
            .catch(() => false),
        { timeout: 15000 },
      )
      .toBe(true);
  } catch (error) {
    console.error("Fixture hub startup:", startupLog);
    throw error;
  }
}
async function make(workspace, name, prompt) {
  const result = await rpc(workspace, "thread.new", {
    project: repo,
    name,
    runtime: "fake",
    mode: "manual",
  });
  const id = result.thread?.id || result.thread || result.id;
  assert.equal(typeof id, "string");
  if (prompt) await rpc(workspace, "thread.send", { thread: id, prompt });
  return id;
}
const metrics = {};
let hubPaused = false;
async function launch() {
  app = await electron.launch({
    args: ["."],
    cwd: resolve("."),
    env,
    timeout: 60000,
  });
  const page = await app.firstWindow();
  page.setDefaultTimeout(10000);
  page.on("pageerror", (error) => errors.push(error.message));
  await page.setViewportSize({ width: 900, height: 900 });
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 30000 });
  return page;
}
async function closeApp() {
  if (!app) return;
  const closing = app;
  app = undefined;
  const bounded = async (operation) => {
    let timer;
    try {
      return await Promise.race([
        operation,
        new Promise((_, reject) => {
          timer = setTimeout(
            () => reject(new Error("Fixture cleanup timed out")),
            2500,
          );
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  };
  await bounded(closing.evaluate(({ app }) => app.exit(0))).catch(() =>
    closing.process()?.kill("SIGKILL"),
  );
  await bounded(closing.close()).catch(() =>
    closing.process()?.kill("SIGKILL"),
  );
}

async function talk(page, name) {
  await page
    .locator(".thread-list .thread-card")
    .filter({ hasText: name })
    .click();
  await page
    .getByRole("button", { name: "Talk to this thread", exact: true })
    .click();
}
async function saved(page) {
  return page.evaluate(async () => {
    const history = JSON.parse(
      (await window.twr.secureRead("offline:local")) || "null",
    );
    const pending = JSON.parse(
      (await window.twr.secureRead("offline-pending:local")) || "null",
    );
    return history || pending
      ? {
          ...history,
          drafts: pending?.drafts || history?.drafts || {},
          outbox: pending?.outbox || history?.outbox || [],
          workspaces: history?.workspaces || {},
        }
      : null;
  });
}
async function pauseHub(page) {
  processes[0].kill("SIGSTOP");
  hubPaused = true;
  const appPid = app.process().pid;
  const children = execFileSync("ps", ["-axo", "pid=,ppid=,args="], {
    encoding: "utf8",
  })
    .split("\n")
    .map((line) => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line))
    .filter(Boolean)
    .filter(
      (match) =>
        Number(match[2]) === appPid && match[3].includes("twr-desktop-bridge"),
    );
  assert(children.length > 0);
  for (const child of children) process.kill(Number(child[1]), "SIGTERM");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Disconnected" })
    .waitFor();
}
async function resumeHub(page) {
  processes[0].kill("SIGCONT");
  hubPaused = false;
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 30000 });
}

async function exerciseReviewControls(page, history) {
  const message = {
    id: randomUUID(),
    workspace: "default",
    method: "thread.send",
    params: { thread: history, prompt: "REVIEW_SAME_ID_MARKER" },
    createdAt: Date.now(),
    state: "review",
    error: "QA: check delivery before retrying",
  };
  await page.evaluate(async (message) => {
    const key = "offline-pending:local";
    const record = JSON.parse(await window.twr.secureRead(key));
    record.outbox.push(message);
    await window.twr.secureWrite(key, JSON.stringify(record));
  }, message);
  await page.reload();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  const outbox = page
    .locator(".outbox-message")
    .filter({ hasText: message.params.prompt });
  await expect(outbox).toContainText("Review delivery");
  const destination = await page.locator(".destination-chip").innerText();
  const currentDraft = await page.locator(".composer textarea").inputValue();
  page.once("dialog", (dialog) => dialog.accept());
  await outbox
    .getByRole("button", { name: "Move to original draft", exact: true })
    .click();
  assert.equal(
    await page.locator(".destination-chip").innerText(),
    destination,
  );
  assert.equal(
    await page.locator(".composer textarea").inputValue(),
    currentDraft,
  );
  await talk(page, "history");
  await expect(page.locator(".composer textarea")).toHaveValue(
    new RegExp(message.params.prompt),
  );
  await expect.poll(async () => (await saved(page)).outbox.length).toBe(0);
  // A separate unsent operation exercises same-ID retry; moving the first one
  // to a draft must have removed it from automatic delivery.
  message.id = randomUUID();
  await page.evaluate(async (message) => {
    const key = "offline-pending:local";
    const record = JSON.parse(await window.twr.secureRead(key));
    record.outbox.push(message);
    await window.twr.secureWrite(key, JSON.stringify(record));
  }, message);
  await page.reload();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await outbox
    .getByRole("button", { name: "Retry with same ID", exact: true })
    .click();
  await expect.poll(async () => (await saved(page))?.outbox.length).toBe(0);
  await page.evaluate((message) => window.twr.sendMessage(message), message);
  const events = (
    await rpc("default", "thread.events", { thread: history, limit: 1000 })
  ).events;
  assert.equal(
    events.filter(
      (event) =>
        event.kind === "turn.started" &&
        event.data.prompt === message.params.prompt,
    ).length,
    1,
  );
  metrics.review = {
    copyPreservedDestination: true,
    copyToOriginalDraft: true,
    retrySameIdDeliveredOnce: true,
    id: message.id,
  };
  console.log(
    "QA: review Copy to original draft preserves destination; Retry same ID delivers exactly once",
  );
}
async function exerciseDefiniteRejection(page) {
  const message = {
    id: randomUUID(),
    workspace: "default",
    method: "thread.send",
    params: {
      thread: "nonexistent-qa-thread",
      prompt: "DEFINITE_REJECTION_MARKER",
    },
    createdAt: Date.now(),
    state: "review",
    error: "QA: retry original operation",
  };
  await page.evaluate(async (message) => {
    const key = "offline-pending:local";
    const record = JSON.parse(await window.twr.secureRead(key));
    record.outbox.push(message);
    await window.twr.secureWrite(key, JSON.stringify(record));
  }, message);
  await page.reload();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  const outbox = page
    .locator(".outbox-message")
    .filter({ hasText: message.params.prompt });
  await outbox
    .getByRole("button", { name: "Retry with same ID", exact: true })
    .click();
  await expect(outbox.locator(".outbox-error")).toContainText(
    "no thread matches",
  );
  await expect
    .poll(
      async () =>
        (await saved(page)).outbox.find((item) => item.id === message.id)
          ?.state,
    )
    .toBe("review");
  assert(!((await saved(page)).outbox[0].error || "").includes("unknown"));
  await outbox
    .getByRole("button", { name: "Retry with same ID", exact: true })
    .click();
  await expect(outbox.locator(".outbox-error")).toContainText(
    "no thread matches",
  );
  assert.equal((await saved(page)).outbox[0].id, message.id);
  page.once("dialog", (dialog) => dialog.accept());
  await outbox
    .getByRole("button", { name: "Remove from outbox", exact: true })
    .click();
  await expect.poll(async () => (await saved(page)).outbox.length).toBe(0);
  metrics.definiteRejection = {
    originalErrorPreserved: true,
    classifiedAsDefinite: true,
    retryKeptId: true,
    removableWithoutReplay: true,
  };
  console.log(
    "QA: definite native rejection preserves original error and same-ID retry",
  );
}
async function checkConfigDraftIsMemoryOnly(page) {
  await page
    .locator(".sidebar-footer")
    .getByRole("button", { name: "Settings", exact: true })
    .click();
  const editor = page.locator("textarea.config-editor");
  await editor.waitFor();
  const original = await editor.inputValue();
  const marker = "CONFIG_DRAFT_MEMORY_ONLY_MARKER";
  await editor.fill(original + "\n# " + marker + "\n");
  await page.waitForTimeout(450);
  const storage = await page.evaluate(async () => ({
    history: await window.twr.secureRead("offline:local"),
    pending: await window.twr.secureRead("offline-pending:local"),
    local: Object.values(localStorage).join("\n"),
  }));
  assert(!JSON.stringify(storage).includes(marker));
  assert(!readFileSync(join(home, "config.toml"), "utf8").includes(marker));
  await app.evaluate(({ dialog }) => {
    globalThis.__qaOriginalEditorDialog = dialog.showMessageBoxSync;
    dialog.showMessageBoxSync = (_window, options) => {
      globalThis.__qaEditorPrompt = options.message;
      return 1;
    };
  });
  await page.reload();
  const prompt = await app.evaluate(({ dialog }) => {
    const prompt = globalThis.__qaEditorPrompt;
    dialog.showMessageBoxSync = globalThis.__qaOriginalEditorDialog;
    return prompt;
  });
  assert.match(prompt, /Discard unsaved editor changes/);
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .locator(".sidebar-footer")
    .getByRole("button", { name: "Settings", exact: true })
    .click();
  await expect(page.locator("textarea.config-editor")).not.toHaveValue(
    new RegExp(marker),
  );
  metrics.configDraft = {
    notInEncryptedJournal: true,
    notInLocalStorage: true,
    notOnDisk: true,
    discardedOnReload: true,
  };
  console.log("QA: unsaved config draft stays in memory only");
}
async function exerciseStorageRecovery() {
  const historyFile = join(
    home,
    "electron",
    "secure",
    "offline",
    createHash("sha256").update("offline:local").digest("hex") + ".enc",
  );
  await closeApp();
  const original = readFileSync(historyFile);
  writeFileSync(historyFile, "CORRUPTED_FIXTURE_CACHE", { mode: 0o600 });
  let page = await launch();
  await page
    .locator(".sidebar-footer")
    .getByRole("button", { name: "Settings", exact: true })
    .click();
  const retry = page.getByRole("button", {
    name: "Retry encrypted storage",
    exact: true,
  });
  await retry.waitFor();
  await retry.click();
  await page.waitForTimeout(100);
  await expect(retry).toBeVisible();
  // Restore only this fixture's original ciphertext, modeling a storage backend
  // becoming available again without touching the user's actual system keychain.
  writeFileSync(historyFile, original, { mode: 0o600 });
  await retry.click();
  await expect(retry).toBeHidden();
  assert((await saved(page)).workspaces.default);
  await closeApp();
  writeFileSync(historyFile, "CORRUPTED_FIXTURE_CACHE_AGAIN", { mode: 0o600 });
  page = await launch();
  await page
    .locator(".sidebar-footer")
    .getByRole("button", { name: "Settings", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Reset offline storage…", exact: true })
    .click();
  let dialog = page.getByRole("dialog", {
    name: "Reset offline storage?",
    exact: true,
  });
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  assert(
    readFileSync(historyFile).includes(
      Buffer.from("CORRUPTED_FIXTURE_CACHE_AGAIN"),
    ),
    "Cancel removed saved storage",
  );
  await page
    .getByRole("button", { name: "Reset offline storage…", exact: true })
    .click();
  dialog = page.getByRole("dialog", {
    name: "Reset offline storage?",
    exact: true,
  });
  await dialog
    .getByRole("button", { name: "Reset offline storage", exact: true })
    .click();
  await expect(dialog).toBeHidden();
  await expect(
    page.getByRole("button", { name: "Retry encrypted storage", exact: true }),
  ).toBeHidden();
  const recovered = await saved(page);
  assert.equal(Object.keys(recovered?.drafts || {}).length, 0);
  assert.equal(recovered?.outbox?.length || 0, 0);
  metrics.storageRecovery = {
    retryAfterNativeDecryptFailure: true,
    resetCancelledWithoutDeletion: true,
    resetClearedDraftsAndOutbox: true,
    actualSystemKeychainUnchanged: true,
  };
  console.log(
    "QA: native encrypted storage error Retry/Reset recover without touching system keychain",
  );
  return page;
}
async function exerciseRemoteForget(page, history) {
  const port = await new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
  const gateway = spawn(
    join(bin, "twr-mobile-gateway"),
    [
      "--development",
      "--pair",
      "--json",
      "--binary-dir",
      bin,
      "--listen",
      `127.0.0.1:${port}`,
    ],
    { env, stdio: ["ignore", "pipe", "pipe"] },
  );
  processes.push(gateway);
  let output = "";
  let gatewayLog = "";
  gateway.stdout.on("data", (chunk) => {
    output += chunk;
  });
  gateway.stderr.on("data", (chunk) => {
    gatewayLog += chunk;
  });
  let invitation;
  await expect
    .poll(
      () => {
        invitation = output
          .split("\n")
          .map((line) => {
            try {
              return JSON.parse(line);
            } catch {}
          })
          .find((value) => value?.code);
        return !!invitation;
      },
      { timeout: 15000, message: gatewayLog },
    )
    .toBe(true);
  let healthError = "";
  try {
    await expect
      .poll(
        () =>
          fetch(`http://127.0.0.1:${port}/health`)
            .then((response) => response.status)
            .catch((error) => {
              healthError = String(error);
              return 0;
            }),
        { timeout: 15000 },
      )
      .toBe(200);
  } catch (error) {
    console.error("Fixture gateway startup:", {
      exit: gateway.exitCode,
      log: gatewayLog,
      healthError,
    });
    throw error;
  }
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  let dialog = page.getByRole("dialog", {
    name: "Hubs and remote access",
    exact: true,
  });
  await dialog
    .getByLabel("Hub name", { exact: true })
    .fill("Disposable forget QA");
  await dialog
    .getByLabel("HTTPS hub URL", { exact: true })
    .fill(`http://127.0.0.1:${port}`);
  await dialog
    .getByLabel("One-use code", { exact: true })
    .fill(invitation.code);
  await dialog
    .getByLabel("This device", { exact: true })
    .fill("Desktop fixture");
  await dialog
    .getByRole("button", { name: "Pair and connect", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Hubs and remote access", exact: true }),
  ).toHaveText("Disposable forget QA");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 30000 });
  const hub = (
    await page.evaluate(() => window.twrRemote.listHubs())
  ).hubs.find((hub) => hub.name === "Disposable forget QA");
  assert(hub);
  await talk(page, "history");
  await page.locator(".composer textarea").fill("REMOTE_FORGET_DRAFT_MARKER");
  const keys = [
    `offline:${hub.id}`,
    `offline-pending:${hub.id}`,
    `bootstrap:${hub.id}`,
  ];
  const records = () =>
    page.evaluate(
      async (keys) =>
        Promise.all(keys.map((key) => window.twrLocal.secureRead(key))),
      keys,
    );
  await expect
    .poll(async () => (await records()).every(Boolean), { timeout: 10000 })
    .toBe(true);
  await expect
    .poll(
      async () => JSON.parse((await records())[1]).drafts["default:" + history],
    )
    .toBe("REMOTE_FORGET_DRAFT_MARKER");
  // Revisit the same saved hub to create another mounted journal before forgetting.
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  dialog = page.getByRole("dialog", {
    name: "Hubs and remote access",
    exact: true,
  });
  await dialog
    .getByRole("button", { name: "This computer", exact: true })
    .click();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  dialog = page.getByRole("dialog", {
    name: "Hubs and remote access",
    exact: true,
  });
  await dialog
    .locator(".saved-hub")
    .filter({ hasText: hub.name })
    .getByRole("button", { name: "Connect", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Hubs and remote access", exact: true }),
  ).toHaveText(hub.name);
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await talk(page, "history");
  await expect(page.locator(".composer textarea")).toHaveValue(
    "REMOTE_FORGET_DRAFT_MARKER",
  );
  gateway.kill("SIGKILL");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Disconnected" })
    .waitFor({ timeout: 15000 });
  await page
    .locator(".composer textarea")
    .fill("REMOTE_FORGET_QUEUE_MUST_NOT_SEND");
  await page
    .getByRole("button", { name: "Save message for later", exact: true })
    .click();
  await expect
    .poll(async () => JSON.parse((await records())[1]).outbox.length)
    .toBe(1);
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  dialog = page.getByRole("dialog", {
    name: "Hubs and remote access",
    exact: true,
  });
  await dialog
    .getByRole("button", { name: "This computer", exact: true })
    .click();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  dialog = page.getByRole("dialog", {
    name: "Hubs and remote access",
    exact: true,
  });
  await dialog
    .locator(".saved-hub")
    .filter({ hasText: hub.name })
    .getByRole("button", { name: "Forget", exact: true })
    .click();
  await expect(
    dialog.locator(".saved-hub").filter({ hasText: hub.name }),
  ).toHaveCount(0);
  await expect.poll(records).toEqual([null, null, null]);
  await page.waitForTimeout(1200);
  assert.deepEqual(
    await records(),
    [null, null, null],
    "A retired journal recreated forgotten data",
  );
  assert.equal(
    (
      await rpc("default", "thread.events", { thread: history, limit: 1000 })
    ).events.filter(
      (event) =>
        event.kind === "turn.started" &&
        event.data.prompt === "REMOTE_FORGET_QUEUE_MUST_NOT_SEND",
    ).length,
    0,
  );
  await dialog
    .getByRole("button", { name: "Close dialog", exact: true })
    .click();
  metrics.remoteForget = {
    actualPairingFixture: true,
    multipleJournalMounts: true,
    deletedHistoryPendingBootstrap: true,
    noLateWrites: true,
    queuedMessageNotDelivered: true,
  };
  console.log(
    "QA: real paired remote Forget clears all encrypted lanes and queued message without late writes",
  );
}
const historyText =
  "CACHED_HISTORY_MARKER\n\nParagraph one wraps safely at large text sizes. Another sentence checks the next line.\n\n```ts\nconst first = 1;\nconst second = 2;\nconsole.log(first + second);\n```";
try {
  await start("default");
  const orchestrator = (
    await rpc("default", "orchestrator.use", { runtime: "codex" })
  ).thread;

  const history = await make("default", "history", historyText);
  const permission = await make(
    "default",
    "permission",
    "!perm Bash echo permission",
  );
  await expect
    .poll(
      async () =>
        (await rpc("default", "thread.get", { thread: history })).status,
    )
    .not.toMatch(/running|waiting/);
  let page = await launch();
  if (process.env.TWR_QA_ONLY === "remote") {
    await exerciseRemoteForget(page, history);
    console.log("PASS: focused paired remote Forget recovery");
  } else {
    const sidebar = page.locator(".sidebar");
    const resize = page.getByRole("separator", {
      name: "Resize sidebar",
      exact: true,
    });
    await resize.focus();
    await resize.press("End");
    await expect
      .poll(async () => Math.round((await sidebar.boundingBox()).width))
      .toBe(306);
    await expect(resize).toHaveAttribute("aria-valuenow", "306");
    assert.equal(
      (
        await page.evaluate(() =>
          JSON.parse(localStorage.getItem("twr.layout.v1")),
        )
      ).sidebar,
      306,
    );
    await resize.press("ArrowLeft");
    await expect
      .poll(async () => Math.round((await sidebar.boundingBox()).width))
      .toBe(296);
    await expect(resize).toHaveAttribute("aria-valuenow", "296");
    assert.equal(
      (
        await page.evaluate(() =>
          JSON.parse(localStorage.getItem("twr.layout.v1")),
        )
      ).sidebar,
      296,
    );
    metrics.sidebar = {
      viewport: 900,
      afterEnd: 306,
      afterLeft: 296,
      aria: Number(await resize.getAttribute("aria-valuenow")),
    };
    await page
      .locator(".sidebar-footer")
      .getByRole("button", { name: "Settings", exact: true })
      .click();
    const options = page.locator(".layout-options");
    const card = page
      .locator(".thread-list .thread-card")
      .filter({ hasText: "history" });
    await card.waitFor();
    const comfortable = (await card.boundingBox()).height;
    await options
      .getByRole("combobox", { name: "Density", exact: true })
      .selectOption("compact");
    await expect
      .poll(async () => (await card.boundingBox()).height)
      .toBeLessThan(comfortable);
    const compact = (await card.boundingBox()).height;
    assert(compact >= 48);
    metrics.density = { comfortable, compact };
    console.log(
      "QA: sidebar306/296 with ARIA/preferences; comfortable/compact card heights",
      comfortable,
      compact,
    );
    await options
      .getByRole("slider", { name: "Interface text", exact: true })
      .focus();
    await options
      .getByRole("slider", { name: "Interface text", exact: true })
      .press("End");
    await options
      .getByRole("slider", { name: "Code & terminal text", exact: true })
      .focus();
    await options
      .getByRole("slider", { name: /Code & terminal text/ })
      .press("End");
    await talk(page, "history");
    await page
      .locator(".block-assistant")
      .filter({ hasText: "CACHED_HISTORY_MARKER" })
      .waitFor();
    await expect
      .poll(() =>
        page
          .locator(".block-assistant .markdown")
          .first()
          .evaluate((node) => parseFloat(getComputedStyle(node).fontSize)),
      )
      .toBe(22);
    await expect
      .poll(() =>
        page
          .locator(".block-assistant .code-block pre")
          .first()
          .evaluate((node) => parseFloat(getComputedStyle(node).fontSize)),
      )
      .toBe(24);
    metrics.typography = await page.evaluate(() => {
      const reading = document.querySelector(".block-assistant .markdown");
      const code = document.querySelector(".block-assistant .code-block pre");
      const readingStyle = getComputedStyle(reading);
      const codeStyle = getComputedStyle(code);
      const cards = [
        ...document.querySelectorAll(".thread-list .thread-card"),
      ].map((node) => {
        const title = node
          .querySelector(".thread-card-title")
          .getBoundingClientRect();
        const runtime = node
          .querySelector(".thread-card-runtime")
          .getBoundingClientRect();
        return { titleBottom: title.bottom, runtimeTop: runtime.top };
      });
      const blocks = [...document.querySelectorAll(".transcript-block")]
        .map((node) => {
          const rect = node.getBoundingClientRect();
          return { top: rect.top, bottom: rect.bottom };
        })
        .sort((a, b) => a.top - b.top);
      return {
        readingSize: parseFloat(readingStyle.fontSize),
        readingLine: parseFloat(readingStyle.lineHeight),
        codeSize: parseFloat(codeStyle.fontSize),
        codeLine: parseFloat(codeStyle.lineHeight),
        pageWidth: document.documentElement.scrollWidth,
        viewport: innerWidth,
        cards,
        blocks,
      };
    });
    assert(metrics.typography.codeLine >= metrics.typography.codeSize * 1.4);
    assert(
      metrics.typography.readingLine >= metrics.typography.readingSize * 1.4,
    );
    assert(metrics.typography.pageWidth <= metrics.typography.viewport + 1);
    for (const item of metrics.typography.cards)
      assert(
        item.titleBottom <= item.runtimeTop + 1,
        "Large text card lines overlap",
      );
    for (let i = 1; i < metrics.typography.blocks.length; i++)
      assert(
        metrics.typography.blocks[i - 1].bottom <=
          metrics.typography.blocks[i].top + 2,
        "Virtual transcript rows overlap at large text size",
      );
    mkdirSync("test-results", { recursive: true });
    await page.screenshot({ path: "test-results/preferences-large-text.png" });
    const draft = "NATIVE_DRAFT_RELOAD_MARKER";
    await page.locator(".composer textarea").fill(draft);
    await expect
      .poll(async () => (await saved(page))?.drafts["default:" + history])
      .toBe(draft);
    await expect
      .poll(async () =>
        (await saved(page))?.workspaces.default?.transcripts[history]?.some(
          (event) => event.data?.text?.includes("CACHED_HISTORY_MARKER"),
        ),
      )
      .toBe(true);
    await page.reload();
    await page
      .locator(".connection-status")
      .filter({ hasText: "Hub connected" })
      .waitFor();
    await talk(page, "history");
    await expect(page.locator(".composer textarea")).toHaveValue(draft);
    assert.equal(
      (
        await page.evaluate(() =>
          JSON.parse(localStorage.getItem("twr.layout.v1")),
        )
      ).textSize,
      22,
    );
    const vault = join(home, "electron", "secure", "offline");
    const encrypted = readdirSync(vault).filter((name) =>
      name.endsWith(".enc"),
    );
    assert(encrypted.length > 0);
    for (const name of encrypted) {
      const path = join(vault, name);
      const bytes = readFileSync(path);
      assert(
        !bytes.includes(Buffer.from(draft)),
        "Native draft leaked into its encrypted file",
      );
      assert(
        !bytes.includes(Buffer.from("CACHED_HISTORY_MARKER")),
        "Cached history leaked into its encrypted file",
      );
      assert.equal(statSync(path).mode & 0o777, 0o600);
    }
    assert(
      !(await page.evaluate(() => Object.values(localStorage))).some((value) =>
        value.includes(draft),
      ),
      "Draft remained in renderer localStorage",
    );
    console.log(
      "QA: large text/code metrics and encrypted draft/cache reload passed",
    );
    metrics.encrypted = {
      files: encrypted.length,
      plaintextAbsent: true,
      mode: "0600",
    };
    // Pause the actual hub and close only this fixture app's companion. Existing reads
    // disconnect and replacement sockets cannot complete a handshake until SIGCONT.
    const permissionBefore = (
      await rpc("default", "attention.list", { all: true })
    ).find((item) => item.thread === permission);
    assert(permissionBefore);
    await pauseHub(page);
    await expect(
      page
        .locator(".block-assistant")
        .filter({ hasText: "CACHED_HISTORY_MARKER" }),
    ).toBeVisible();
    await page
      .locator(".thread-list .thread-card")
      .filter({ hasText: "permission" })
      .click();
    const approval = page.locator(".inspector .attention-card");
    await approval.waitFor();
    for (const button of await approval
      .locator(".attention-options button")
      .all())
      await expect(button).toBeDisabled();
    await approval.focus();
    await page.keyboard.press("1");
    await page.screenshot({
      path: "test-results/preferences-offline-readonly.png",
    });
    await talk(page, "history");
    await expect(page.locator(".composer textarea")).toBeEnabled();
    const queuedText = "OFFLINE_DELIVER_EXACTLY_ONCE_MARKER";
    await page.locator(".composer textarea").fill(queuedText);
    await page
      .getByRole("button", { name: "Save message for later", exact: true })
      .click();
    await expect.poll(async () => (await saved(page))?.outbox.length).toBe(1);
    const queued = (await saved(page)).outbox[0];
    assert.match(queued.id, /^[0-9a-f]{8}-[0-9a-f-]{27}$/i);
    assert.equal(queued.workspace, "default");
    assert.equal(queued.method, "thread.send");
    assert.equal(queued.params.thread, history);
    await expect(
      page.locator(".outbox-message").filter({ hasText: queuedText }),
    ).toContainText(queuedText);
    await page
      .locator(".workspace-entry.current .workspace-orchestrator")
      .click();
    await page.locator(".composer textarea").fill("yes");
    await page
      .getByRole("button", { name: "Save message for later", exact: true })
      .click();
    await expect.poll(async () => (await saved(page))?.outbox.length).toBe(2);
    const yes = (await saved(page)).outbox.find(
      (item) => item.method === "say",
    );
    assert(yes);
    assert(!Object.hasOwn(yes.params, "seen_gseq"));
    assert(!Object.hasOwn(yes.params, "focused"));
    // Old releases could persist answer authority. Even a legacy record must lose it
    // on load, regardless of the previously displayed permission revision.
    await page.evaluate(async (id) => {
      const key = "offline-pending:local";
      const record = JSON.parse(await window.twr.secureRead(key));
      record.outbox = record.outbox.map((item) =>
        item.id === id
          ? {
              ...item,
              params: {
                ...item.params,
                seen_gseq: 999999999,
                focused: "permission",
              },
            }
          : item,
      );
      await window.twr.secureWrite(key, JSON.stringify(record));
    }, yes.id);
    await page.reload();
    await expect
      .poll(async () => (await saved(page))?.outbox[0]?.id)
      .toBe(queued.id);
    await expect
      .poll(
        async () =>
          (await saved(page))?.outbox.find((item) => item.id === yes.id)?.id,
      )
      .toBe(yes.id);
    await expect(
      page.locator(".outbox-message").filter({ hasText: queuedText }),
    ).toContainText(queuedText);
    await resumeHub(page);
    await expect
      .poll(async () => (await saved(page))?.outbox.length, { timeout: 15000 })
      .toBe(0);
    await expect(page.locator(".composer textarea")).toHaveValue("yes");
    assert.equal((await saved(page)).drafts["default:orchestrator"], "yes");
    const permissionAfter = (
      await rpc("default", "attention.list", { all: true })
    ).find((item) => item.attn_id === permissionBefore.attn_id);
    assert.equal(
      permissionAfter?.revision,
      permissionBefore.revision,
      "Offline yes changed or answered the permission",
    );
    assert.equal(
      (
        await rpc("default", "thread.events", {
          thread: orchestrator,
          limit: 1000,
        })
      ).events.filter(
        (event) => event.kind === "turn.started" && event.data.prompt === "yes",
      ).length,
      0,
    );
    metrics.offlineAnswer = {
      samePermissionRevision: true,
      restoredDraft: "yes",
      outboxEmpty: true,
      orchestratorTurns: 0,
      legacyAuthorityRemoved: true,
    };
    console.log(
      "QA: offline yes survived reload without answering or reaching orchestrator; restored to draft",
    );
    const countDeliveries = async () =>
      (
        await rpc("default", "thread.events", { thread: history, limit: 1000 })
      ).events.filter(
        (event) =>
          event.kind === "turn.started" && event.data.prompt === queuedText,
      ).length;
    await expect.poll(countDeliveries).toBe(1);
    // A repeated delivery carries the same UUID; the native encrypted receipt returns
    // the original reply instead of sending the plain message to the hub again.
    await page.evaluate((message) => window.twr.sendMessage(message), queued);
    assert.equal(await countDeliveries(), 1);
    await talk(page, "history");
    const restartDraft = "NATIVE_DRAFT_RESTART_MARKER";
    await page.locator(".composer textarea").fill(restartDraft);
    await expect
      .poll(async () => (await saved(page))?.drafts["default:" + history])
      .toBe(restartDraft);
    await closeApp();
    page = await launch();
    await talk(page, "history");
    await expect(page.locator(".composer textarea")).toHaveValue(restartDraft);
    assert(
      (await saved(page)).workspaces.default.transcripts[history].length > 0,
    );
    await page.evaluate((message) => window.twr.sendMessage(message), queued);
    assert.equal(await countDeliveries(), 1);
    assert.equal(
      (await rpc("default", "attention.list", { all: true })).length,
      1,
      "Cached permission shortcut answered a live request",
    );
    await exerciseReviewControls(page, history);
    await exerciseDefiniteRejection(page);
    await checkConfigDraftIsMemoryOnly(page);
    page = await exerciseStorageRecovery();
    await exerciseRemoteForget(page, history);
    metrics.outbox = {
      id: queued.id,
      stableAcrossReload: true,
      deliveriesAfterReplayAndRestart: 1,
    };
    writeFileSync(
      "test-results/preferences-offline-qa.json",
      JSON.stringify(metrics, null, 2) + "\n",
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: layout and encrypted native recovery; offline yes cannot answer; stable UUID/receipt retries; review Copy/Retry; definite errors; config drafts stay in memory; storage Retry/Reset; paired remote Forget deletes all cache lanes without late writes",
    );
  }
} catch (error) {
  if (app) {
    const page = await app.firstWindow();
    console.error(
      await page
        .locator("body")
        .innerText()
        .catch(() => "<closed>"),
    );
    mkdirSync("test-results", { recursive: true });
    await page
      .screenshot({ path: "test-results/preferences-offline-failure.png" })
      .catch(() => {});
  }
  throw error;
} finally {
  if (hubPaused) processes[0]?.kill("SIGCONT");
  await closeApp();
  try {
    execFileSync(join(bin, "twr"), ["hub", "stop"], {
      env,
      stdio: "ignore",
      timeout: 10000,
    });
  } catch {}
  for (const process of processes)
    if (process.exitCode === null && process.signalCode === null)
      process.kill("SIGTERM");
  stopFixture(join(bin, "twr"), env, { workspaces: [] });
  rmSync(home, {
    recursive: true,
    force: true,
    maxRetries: 10,
    retryDelay: 100,
  });
}
