// Focused Electron + Rust QA. All hub processes and SQLite rows are disposable.
import { clientProtocol } from "./client-protocol.mjs";
import { _electron as electron, expect } from "@playwright/test";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createConnection } from "node:net";
import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
const home = mkdtempSync(join(tmpdir(), "twr-inbox-search-"));
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
  child.stderr.resume();
  await expect
    .poll(
      () =>
        rpc(workspace, "hub.info")
          .then(() => true)
          .catch(() => false),
      { timeout: 15000 },
    )
    .toBe(true);
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
async function assertReadOnlyCompanion() {
  const child = spawn(join(bin, "twr-desktop-bridge"), ["default"], {
    env: { ...env, TWR_BRIDGE_READ_ONLY: "1" },
    stdio: "pipe",
  });
  const pending = new Map();
  const events = [];
  let id = 0;
  const lines = createInterface({ input: child.stdout });
  lines.on("line", (line) => {
    const message = JSON.parse(line);
    if (message.id) {
      pending.get(message.id)?.(message);
      pending.delete(message.id);
    } else events.push(message);
  });
  const call = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const next = ++id;
      const timer = setTimeout(
        () => reject(new Error("Inbox companion timed out")),
        5000,
      );
      pending.set(next, (message) => {
        clearTimeout(timer);
        resolve(message);
      });
      child.stdin.write(JSON.stringify({ id: next, method, params }) + "\n");
    });
  try {
    assert.equal((await call("hub.info")).result.workspace, "default");
    assert(Array.isArray((await call("attention.list", { all: true })).result));
    for (const method of [
      "notify.looking",
      "control.subscribe",
      "desktop.ensure_hub",
      "thread.send",
    ])
      assert.match((await call(method)).error, /only read workspace summaries/);
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(
      events,
      [],
      "Inbox-only companion must not subscribe or emit active state",
    );
  } finally {
    child.stdin.end();
    child.kill("SIGTERM");
    lines.close();
  }
}
try {
  await start("default");
  execFileSync(join(bin, "twr"), ["ws", "new", "other"], {
    env,
    stdio: "ignore",
  });
  await start("other");
  execFileSync(join(bin, "twr"), ["ws", "new", "sleeping"], {
    env,
    stdio: "ignore",
  });
  const history = await make(
    "default",
    "history",
    "!tool Read SEARCH_TOOL_INPUT\nHISTORY_MESSAGE",
  );
  await expect
    .poll(
      async () =>
        (await rpc("default", "thread.get", { thread: history })).status,
    )
    .not.toMatch(/running|waiting/);
  const archiveId = "00000000000000000000000001";
  execFileSync("python3", [
    "-c",
    `
import sqlite3,json,sys
from pathlib import Path
root=Path(sys.argv[1]);thread=sys.argv[2];selected=sys.argv[3]
hot=sqlite3.connect(root/'data'/'twr.db')
columns='thread,seq,id,v,ts,turn,source,kind,data,raw'
for n in range(513):
 hot.execute(f'INSERT INTO events ({columns}) VALUES (?,?,?,?,?,?,?,?,?,?)',(thread,10000+n,'7ZZZZZZZZZ'+str(n).zfill(16),1,'2026-11-06T12:00:00.000Z',1,'adapter:fake','agent.text',json.dumps({'text':'unrelated filler '+str(n),'final':True}),None))
for n in range(620):
 hot.execute(f'INSERT INTO events ({columns}) VALUES (?,?,?,?,?,?,?,?,?,?)',(thread,20000+n,'7ZZZZZZZZY'+str(n).zfill(16),1,'2026-11-06T12:01:00.000Z',1,'adapter:fake','agent.text',json.dumps({'text':'streaming delta only','delta':True,'final':False}),None))
hot.commit()
archive=sqlite3.connect(root/'data'/'archive.db')
archive.execute('CREATE TABLE events (thread TEXT NOT NULL,seq INTEGER NOT NULL,id TEXT NOT NULL UNIQUE,v INTEGER NOT NULL,ts TEXT NOT NULL,turn INTEGER,source TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL,raw TEXT,PRIMARY KEY(thread,seq))')
archive.execute('CREATE TABLE blobs (hash TEXT PRIMARY KEY,bytes BLOB NOT NULL)')
archive.execute('PRAGMA user_version=1')
archive.execute(f'INSERT INTO events ({columns}) VALUES (?,?,?,?,?,?,?,?,?,?)',(thread,9999,selected,1,'2026-11-06T11:59:00.000Z',1,'adapter:fake','agent.tool_result',json.dumps({'call_id':'archived-call','ok':True,'output_summary':'ARCHIVED_RESULT_MARKER exact tool output'}),None))
archive.commit()
`,
    home,
    history,
    archiveId,
  ]);
  const first = await rpc("default", "conversation.search", {
    query: "ARCHIVED_RESULT_MARKER",
    thread: history,
  });
  assert.equal(first.items.length, 0);
  assert.equal(first.more, true);
  assert(first.next);
  const second = await rpc("default", "conversation.search", {
    query: "ARCHIVED_RESULT_MARKER",
    thread: history,
    before: first.next,
  });
  assert.equal(second.items[0].event_id, archiveId);
  assert.equal(
    (
      await rpc("default", "thread.event_context", {
        thread: history,
        event_id: archiveId,
      })
    ).event.id,
    archiveId,
  );
  await make("default", "permission-a", "!perm Bash echo a");
  await make("default", "permission-b", "!perm Bash echo b");
  await make("default", "question", "!ask LOCAL_QUESTION|Yes|Later");
  await make("other", "elsewhere", "!ask OTHER_WORKSPACE_QUESTION|Yes|Later");
  await expect
    .poll(
      async () =>
        (await rpc("default", "attention.list", { all: true })).length,
    )
    .toBeGreaterThanOrEqual(3);
  await assertReadOnlyCompanion();
  app = await electron.launch({
    args: ["."],
    cwd: resolve("."),
    env,
    timeout: 60000,
  });
  const page = await app.firstWindow();
  page.on("pageerror", (error) => errors.push(error.message));
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 30000 });
  await app.evaluate(() => {
    const childProcess = process.getBuiltinModule("node:child_process");
    globalThis.__qaOriginalSpawn = childProcess.spawn;
    globalThis.__qaInboxReaders = 0;
    childProcess.spawn = (...args) => {
      if (
        String(args[0]).endsWith("twr-desktop-bridge") &&
        args[2]?.env?.TWR_BRIDGE_READ_ONLY === "1"
      )
        globalThis.__qaInboxReaders++;
      return Reflect.apply(globalThis.__qaOriginalSpawn, childProcess, args);
    };
  });
  const concurrent = await page.evaluate(() =>
    Promise.all([
      window.twr.inboxSources(),
      window.twr.inboxSources(),
      window.twr.inboxSources(),
    ]),
  );
  const readers = await app.evaluate(() => {
    const childProcess = process.getBuiltinModule("node:child_process");
    childProcess.spawn = globalThis.__qaOriginalSpawn;
    return globalThis.__qaInboxReaders;
  });
  assert.equal(
    readers,
    2,
    "Concurrent refreshes must share one read scan and skip stopped hubs",
  );
  assert.deepEqual(concurrent[0], concurrent[1]);
  assert.deepEqual(concurrent[1], concurrent[2]);
  assert.equal(
    concurrent[0].find((source) => source.workspace === "sleeping").running,
    false,
  );
  await page
    .locator(".thread-list .thread-card")
    .filter({ hasText: "history" })
    .click();
  await page
    .getByRole("button", { name: "Talk to this thread", exact: true })
    .click();
  const composer = page.locator(".composer textarea");
  await composer.fill("KEEP_MY_DESTINATION_AND_DRAFT");
  const destination = await page.locator(".destination-chip").innerText();
  await page
    .getByRole("button", { name: "Search conversations", exact: true })
    .click();
  const search = page.getByRole("dialog", {
    name: "Search conversations",
    exact: true,
  });
  await search.locator("select").selectOption(history);
  await search
    .getByRole("textbox", {
      name: "Search messages, tool output, and decisions",
    })
    .fill("ARCHIVED_RESULT_MARKER");
  await expect(search.getByRole("status")).toContainText(
    "No matches in these events. Search older events to continue.",
  );
  await search
    .getByRole("button", { name: "Search older events", exact: true })
    .click();
  await search
    .locator(".search-hit")
    .filter({ hasText: "ARCHIVED_RESULT_MARKER" })
    .click();
  const context = page.getByRole("dialog", {
    name: "Conversation event",
    exact: true,
  });
  await expect(context.locator(".search-event.selected")).toContainText(
    "ARCHIVED_RESULT_MARKER exact tool output",
  );
  await expect(context.locator(".search-event.selected")).toContainText(
    "#9999",
  );
  await expect(context).toContainText("This computer / default");
  await context
    .getByRole("button", { name: "Close dialog", exact: true })
    .click();
  assert.equal(await composer.inputValue(), "KEEP_MY_DESTINATION_AND_DRAFT");
  assert.equal(
    await page.locator(".destination-chip").innerText(),
    destination,
  );
  await page.keyboard.press(
    process.platform === "darwin" ? "Meta+f" : "Control+f",
  );
  await page
    .getByRole("dialog", { name: "Search conversations", exact: true })
    .waitFor();
  await page.getByRole("button", { name: "Close dialog", exact: true }).click();
  const platform = await page.evaluate(() => {
    const value = document.documentElement.dataset.platform;
    document.documentElement.dataset.platform = "linux";
    return value;
  });
  const focusFilter = page.getByRole("button", {
    name: "Toggle focus filter",
    exact: true,
  });
  const focusClass = await focusFilter.getAttribute("class");
  await page.keyboard.press("Control+Shift+f");
  await expect(focusFilter).not.toHaveClass(focusClass);
  assert.equal(await page.getByRole("dialog").count(), 0);
  await page.keyboard.press("Control+f");
  await page
    .getByRole("dialog", { name: "Search conversations", exact: true })
    .waitFor();
  await page.getByRole("button", { name: "Close dialog", exact: true }).click();
  await page.keyboard.press("Control+Shift+f");
  await expect(focusFilter).toHaveClass(focusClass);
  await page.evaluate((platform) => {
    document.documentElement.dataset.platform = platform;
  }, platform);
  await page
    .locator(".primary-nav")
    .getByRole("button", { name: /Needs you/ })
    .click();
  const inbox = page.locator(".needs-inbox");
  const stopped = inbox.locator(".inbox-group").filter({ hasText: "sleeping" });
  await expect(stopped).toContainText("Not running");
  await expect(
    stopped.getByRole("button", { name: "Open workspace", exact: true }),
  ).toBeVisible();
  await inbox.getByRole("button", { name: "Completions", exact: true }).click();
  await expect(
    inbox.getByRole("button", { name: "Next request", exact: true }),
  ).toBeDisabled();
  const prevented = await inbox
    .getByRole("button", { name: "Completions", exact: true })
    .evaluate((node) => {
      const event = new KeyboardEvent("keydown", {
        key: "ArrowDown",
        bubbles: true,
        cancelable: true,
      });
      node.dispatchEvent(event);
      return event.defaultPrevented;
    });
  assert.equal(
    prevented,
    false,
    "Empty filters must retain native arrow-key behavior",
  );
  await inbox.getByRole("button", { name: "Permissions", exact: true }).click();
  await expect(inbox.locator(".attention-card")).toHaveCount(2);
  await inbox
    .getByRole("button", { name: "Next request", exact: true })
    .click();
  const focused = () =>
    page.evaluate(() =>
      document.activeElement
        ?.closest("[data-inbox-key]")
        ?.getAttribute("data-inbox-key"),
    );
  const initialFocus = await focused();
  assert(initialFocus);
  await page.keyboard.press("j");
  const nextFocus = await focused();
  assert(nextFocus && nextFocus !== initialFocus);
  await page.keyboard.press("k");
  assert.equal(await focused(), initialFocus);
  await page.setViewportSize({ width: 390, height: 844 });
  await inbox
    .getByRole("button", { name: "Next request", exact: true })
    .click();
  assert.equal(await focused(), nextFocus);
  await inbox.getByRole("button", { name: "Questions", exact: true }).click();
  await expect(inbox.locator(".attention-card")).toHaveCount(1);
  const summary = inbox
    .locator(".inbox-source-request")
    .filter({ hasText: "OTHER_WORKSPACE_QUESTION" });
  await summary.waitFor();
  assert.equal(await summary.locator(".attention-options").count(), 0);
  assert.equal(
    (await page.evaluate(() => window.twr.bootstrap())).workspace,
    "default",
  );
  assert.equal((await rpc("other", "attention.list", { all: true })).length, 1);
  await summary.click();
  await expect
    .poll(
      async () => (await page.evaluate(() => window.twr.bootstrap())).workspace,
    )
    .toBe("other");
  const live = page
    .locator(".needs-inbox .attention-card")
    .filter({ hasText: "OTHER_WORKSPACE_QUESTION" });
  await live.getByRole("button", { name: /Later/ }).click();
  await expect
    .poll(
      async () => (await rpc("other", "attention.list", { all: true })).length,
    )
    .toBe(0);
  mkdirSync("test-results", { recursive: true });
  await page.screenshot({ path: "test-results/inbox-search-qa.png" });
  assert.deepEqual(errors, []);
  console.log(
    "PASS: real Rust archived tool-result search, SQL delta exclusion, empty-page progress/cursor pagination, exact event context, preserved draft/destination, Cmd/Ctrl-F plus Linux Ctrl-Shift-F, coalesced bounded inbox reads, stopped-hub action, empty-filter arrows, permission/question filters, keyboard+touch navigation, workspace summaries require connection before responding",
  );
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
      .screenshot({ path: "test-results/inbox-search-failure.png" })
      .catch(() => {});
  }
  throw error;
} finally {
  if (app) {
    await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
    await app.close().catch(() => {});
  }
  for (const workspace of ["default", "other"]) {
    try {
      execFileSync(join(bin, "twr"), ["-w", workspace, "hub", "stop"], {
        env,
        stdio: "ignore",
        timeout: 10000,
      });
    } catch {}
  }
  for (const process of processes)
    if (process.exitCode === null && process.signalCode === null)
      process.kill("SIGTERM");
  rmSync(home, {
    recursive: true,
    force: true,
    maxRetries: 10,
    retryDelay: 100,
  });
}
