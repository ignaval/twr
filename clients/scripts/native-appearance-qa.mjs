// Verify real native Electron appearance and persistence against an isolated hub.
// Run after `npm run build`: node scripts/native-appearance-qa.mjs.
import { _electron as electron, expect } from "@playwright/test";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  existsSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
const fixture = mkdtempSync(join(tmpdir(), "twr-native-appearance-"));
const hubHome = join(fixture, "hub");
const userData = join(fixture, "profile");
mkdirSync(hubHome);
mkdirSync(userData);
const token = join(fixture, "token");
writeFileSync(token, randomUUID(), { mode: 0o600 });
writeFileSync(
  join(hubHome, "config.toml"),
  'default_runtime="fake"\n[orchestrator]\nruntime="fake"\nautostart=false\n[tui]\nsuggest=false\nsummaries=false\n',
);
writeFileSync(
  join(userData, "desktop.json"),
  JSON.stringify({ workspace: "default", retainedSetting: "keep-me" }),
);
const env = {
  ...process.env,
  HOME: hubHome,
  TWR_HOME: hubHome,
  TWR_TEST_USER_TOKEN_FILE: token,
  TWR_ENABLE_FAKE: "1",
  TWR_DESKTOP_TEST_USER_DATA: userData,
  TWR_DESKTOP_BINARY_DIR: resolve("../target/debug"),
  TWR_WORKSPACE: "default",
  SHELL: "/bin/sh",
};
for (const name of [
  "ELECTRON_RUN_AS_NODE",
  "NODE_OPTIONS",
  "NODE_PATH",
  "TWR_AGENT",
  "TWR_ORCH_TOKEN",
])
  delete env[name];
let application;
const hub = spawn(join(env.TWR_DESKTOP_BINARY_DIR, "twr"), ["hub"], {
  env,
  stdio: ["ignore", "ignore", "pipe"],
});
let hubError = "";
hub.stderr.on("data", (data) => (hubError += data));
hub.on("error", (error) => (hubError += String(error)));
const snapshots = [];
const launch = async () => {
  application = await electron.launch({
    args: ["."],
    cwd: resolve("."),
    env,
    // Playwright otherwise forces prefers-color-scheme:light and masks the
    // actual Electron/native preference when testing System.
    colorScheme: null,
    timeout: 60000,
  });
  const page = await application.firstWindow();
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 60000 });
  assert.equal(
    (await page.evaluate(() => window.twr.rpc("hub.info"))).pid,
    hub.pid,
  );
  return page;
};
const native = () =>
  application.evaluate(({ nativeTheme, BrowserWindow }) => ({
    source: nativeTheme.themeSource,
    dark: nativeTheme.shouldUseDarkColors,
    background: BrowserWindow.getAllWindows()[0]
      .getBackgroundColor()
      .toLowerCase(),
  }));
const settings = () =>
  JSON.parse(readFileSync(join(userData, "desktop.json"), "utf8"));
const verify = async (page, preference) => {
  await expect
    .poll(
      async () => {
        const state = await native();
        const rendered = await page.evaluate(() => ({
          background: getComputedStyle(document.documentElement)
            .getPropertyValue("--bg")
            .trim()
            .toLowerCase(),
          dark:
            getComputedStyle(document.documentElement).colorScheme === "dark",
        }));
        const saved = settings();
        return {
          source: state.source,
          contrastMatches: state.dark === rendered.dark,
          backgroundMatches: state.background === rendered.background,
          savedAppearanceMatches:
            saved.appearance?.preference === preference &&
            saved.appearance?.background === rendered.background &&
            saved.appearance?.colorScheme ===
              (rendered.dark ? "dark" : "light"),
          workspace: saved.workspace,
          retainedSetting: saved.retainedSetting,
        };
      },
      { timeout: 10000 },
    )
    .toEqual({
      source: preference,
      contrastMatches: true,
      backgroundMatches: true,
      savedAppearanceMatches: true,
      workspace: "default",
      retainedSetting: "keep-me",
    });
  const state = await native();
  if (preference !== "system") assert.equal(state.dark, preference === "dark");
  snapshots.push({ preference, ...state });
};
try {
  for (let n = 0; !existsSync(join(hubHome, "hub.sock")); n++) {
    assert(
      n < 300 && hub.exitCode === null,
      "Fixture hub did not start: " + hubError,
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  let page = await launch();
  await verify(page, "dark");
  await page
    .locator(".sidebar-footer")
    .getByRole("button", { name: "Settings", exact: true })
    .click();
  await page.getByRole("button", { name: "Light", exact: true }).click();
  await verify(page, "light");
  await page.screenshot({
    path: "test-results/desktop-native-appearance-light.png",
  });
  const rejected = await page.evaluate(async () => {
    try {
      await window.twr.setAppearance({
        preference: "light",
        colorScheme: "light",
        background: "url(file:///not-allowed)",
      });
      return false;
    } catch {
      return true;
    }
  });
  assert(rejected, "Non-hex native color must be rejected");
  await verify(page, "light");
  await page.getByRole("button", { name: "System", exact: true }).click();
  await verify(page, "system");
  await page.getByRole("button", { name: "Dark", exact: true }).click();
  await verify(page, "dark");
  await page.screenshot({
    path: "test-results/desktop-native-appearance-dark.png",
  });
  await page.getByRole("button", { name: "Light", exact: true }).click();
  await verify(page, "light");
  await page.evaluate(() => window.twr.connect("default"));
  await verify(page, "light");
  await application.close();
  application = undefined;
  page = await launch();
  await verify(page, "light");
  const report = {
    date: new Date().toISOString(),
    platform: process.platform,
    snapshots,
    rejectedInvalidColor: rejected,
    workspaceAndUnknownSettingsPreserved: true,
    persistedAfterRestart: true,
  };
  writeFileSync(
    "test-results/desktop-native-appearance-qa.json",
    JSON.stringify(report, null, 2) + "\n",
  );
  console.log(
    "PASS: native default Dark, Settings Light/Dark/System, actual nativeTheme+window colors, invalid-color rejection, workspace setting preservation and restart persistence",
  );
  console.log(JSON.stringify(report));
} finally {
  await application?.close().catch(() => {});
  if (hub.exitCode === null) {
    hub.kill("SIGTERM");
    await Promise.race([
      new Promise((resolve) => hub.once("exit", resolve)),
      new Promise((resolve) => setTimeout(resolve, 1000)),
    ]);
    if (hub.exitCode === null) hub.kill("SIGKILL");
  }
  rmSync(fixture, { recursive: true, force: true });
}
