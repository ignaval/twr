// Run from clients after `npm run build`, inside a real Linux Hyprland session:
// node scripts/omarchy-qa.mjs
// All hubs, credentials, settings and Omarchy themes live in a disposable HOME.
import { _electron as electron, expect } from "@playwright/test";
import { stopFixture } from "./fixture-cleanup.mjs";
import { spawn, execFileSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  renameSync,
  existsSync,
  rmSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { randomUUID } from "node:crypto";
import assert from "node:assert/strict";

assert.equal(
  process.platform,
  "linux",
  "Omarchy QA requires an actual Linux host",
);
assert(
  process.env.WAYLAND_DISPLAY && process.env.HYPRLAND_INSTANCE_SIGNATURE,
  "Run Omarchy QA inside a real Hyprland/Wayland session",
);
const palettes = {
  dark: {
    mode: "dark",
    background: "#111217",
    foreground: "#e7e7ea",
    accent: "#67d2e0",
    red: "#f28b82",
    yellow: "#e3b86b",
    selection: "#2d3540",
  },
  light: {
    mode: "light",
    background: "#faf9f5",
    foreground: "#26272b",
    accent: "#076b77",
    red: "#a13236",
    yellow: "#8d6600",
    selection: "#e0e6e8",
  },
};
const fixture = mkdtempSync(join(tmpdir(), "twr-omarchy-"));
const hubHome = join(fixture, "home"),
  userData = join(fixture, "profile"),
  repo = join(fixture, "repo");
const themeDirectory = join(hubHome, ".config/omarchy/current/theme"),
  themeFile = join(themeDirectory, "colors.toml");
for (const directory of [hubHome, userData, repo, themeDirectory])
  mkdirSync(directory, { recursive: true });
const writeTheme = (palette) => {
  const temporary = themeFile + ".tmp";
  writeFileSync(
    temporary,
    Object.entries(palette)
      .map(([key, value]) => `${key} = ${JSON.stringify(value)}`)
      .join("\n") + "\n",
  );
  renameSync(temporary, themeFile);
};
writeTheme(palettes.dark);
execFileSync("git", ["init", "-b", "main", repo], { stdio: "ignore" });
const token = join(fixture, "token");
writeFileSync(token, randomUUID(), { mode: 0o600 });
writeFileSync(
  join(hubHome, "config.toml"),
  `default_runtime="fake"\n[projects.demo]\nrepo=${JSON.stringify(repo)}\n[orchestrator]\nruntime="fake"\nautostart=false\n[tui]\nsuggest=false\nsummaries=false\n`,
);
const env = {
  ...process.env,
  HOME: hubHome,
  XDG_CONFIG_HOME: join(hubHome, ".config"),
  TWR_HOME: hubHome,
  TWR_TEST_USER_TOKEN_FILE: token,
  TWR_ENABLE_FAKE: "1",
  TWR_DESKTOP_TEST_USER_DATA: userData,
  TWR_DESKTOP_BINARY_DIR:
    process.env.TWR_DESKTOP_BINARY_DIR || resolve("../target/debug"),
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
const hub = spawn(join(env.TWR_DESKTOP_BINARY_DIR, "twr"), ["hub"], {
  env,
  stdio: ["ignore", "ignore", "pipe"],
});
let hubError = "",
  application,
  page;
hub.stderr.on("data", (data) => (hubError += data));
hub.on("error", (error) => (hubError += String(error)));
const errors = [],
  snapshots = [],
  captures = [],
  switches = [];
mkdirSync("test-results", { recursive: true });
const nativeState = () =>
  application.evaluate(({ nativeTheme, BrowserWindow }) => ({
    source: nativeTheme.themeSource,
    dark: nativeTheme.shouldUseDarkColors,
    background: BrowserWindow.getAllWindows()[0]
      .getBackgroundColor()
      .toLowerCase(),
  }));
const savedSettings = () =>
  JSON.parse(readFileSync(join(userData, "desktop.json"), "utf8"));
const readUI = () =>
  page.evaluate(() => {
    const root = document.documentElement,
      style = getComputedStyle(root),
      probe = document.createElement("span"),
      canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const context = canvas.getContext("2d", { willReadFrequently: true });
    document.body.append(probe);
    const hex = (color) => {
      context.clearRect(0, 0, 1, 1);
      context.fillStyle = color;
      context.fillRect(0, 0, 1, 1);
      return (
        "#" +
        [...context.getImageData(0, 0, 1, 1).data]
          .slice(0, 3)
          .map((value) => value.toString(16).padStart(2, "0"))
          .join("")
      );
    };
    const tokens = {};
    for (const token of [
      "panel",
      "overlay",
      "active",
      "border-strong",
      "bubble",
      "muted",
      "faint",
      "accent",
      "accent-fg",
      "raised",
      "hover",
      "warning",
      "danger",
      "danger-fg",
      "success",
      "loop",
      "field-border",
    ]) {
      probe.style.backgroundColor = `var(--${token})`;
      tokens[token] = hex(getComputedStyle(probe).backgroundColor);
    }
    probe.remove();
    return {
      preference: root.dataset.theme,
      omarchy: root.dataset.omarchy,
      scheme: style.colorScheme,
      background: hex(style.backgroundColor),
      text: hex(style.color),
      tokens,
      loadedBackground: root.style
        .getPropertyValue("--omarchy-bg")
        .trim()
        .toLowerCase(),
    };
  });
const luminance = (hex) => {
  const channels = [1, 3, 5]
    .map((index) => parseInt(hex.slice(index, index + 2), 16) / 255)
    .map((value) =>
      value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4,
    );
  return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
};
const contrast = (a, b) =>
  (Math.max(luminance(a), luminance(b)) + 0.05) /
  (Math.min(luminance(a), luminance(b)) + 0.05);
const readability = (ui) => {
  const values = {};
  for (const surface of [
    "background",
    "panel",
    "raised",
    "overlay",
    "hover",
    "active",
    "bubble",
  ]) {
    const background =
      surface === "background" ? ui.background : ui.tokens[surface];
    // A field's edge reads at 3:1 on what is around it (shared/omarchy.ts).
    const edge = contrast(ui.tokens["field-border"], background);
    values[`field-border/${surface}`] = Math.round(edge * 100) / 100;
    assert(
      edge >= 3,
      `${ui.scheme} field edge on ${surface} contrast ${edge.toFixed(2)} is below 3:1`,
    );
    for (const text of [
      "text",
      "muted",
      "faint",
      "accent",
      "warning",
      "danger",
      "success",
      "loop",
    ]) {
      const foreground = text === "text" ? ui.text : ui.tokens[text];
      const ratio = contrast(foreground, background);
      values[`${text}/${surface}`] = Math.round(ratio * 100) / 100;
      assert(
        ratio >= 4.5,
        `${ui.scheme} ${text} on ${surface} contrast ${ratio.toFixed(2)} is below 4.5:1`,
      );
    }
  }
  values.primary =
    Math.round(contrast(ui.tokens["accent-fg"], ui.tokens.accent) * 100) / 100;
  assert(
    values.primary >= 4.5,
    `${ui.scheme} primary-button contrast is below 4.5:1`,
  );
  values.danger =
    Math.round(contrast(ui.tokens["danger-fg"], ui.tokens.danger) * 100) / 100;
  assert(
    values.danger >= 4.5,
    `${ui.scheme} danger-button contrast is below 4.5:1`,
  );
  return values;
};
const verifySystem = async (palette) => {
  await expect
    .poll(
      async () => {
        const ui = await readUI(),
          native = await nativeState(),
          saved = savedSettings();
        return {
          preference: ui.preference,
          omarchy: ui.omarchy,
          scheme: ui.scheme,
          background: ui.background,
          text: ui.text,
          nativeSource: native.source,
          nativeDark: native.dark,
          nativeBackground: native.background,
          savedPreference: saved.appearance?.preference,
          savedBackground: saved.appearance?.background,
          workspace: saved.workspace,
        };
      },
      { timeout: 30000 },
    )
    .toEqual({
      preference: "system",
      omarchy: "true",
      scheme: palette.mode,
      background: palette.background,
      text: palette.foreground,
      nativeSource: palette.mode,
      nativeDark: palette.mode === "dark",
      nativeBackground: palette.background,
      savedPreference: "system",
      savedBackground: palette.background,
      workspace: "default",
    });
  const ui = await readUI(),
    ratios = readability(ui);
  snapshots.push({ ...ui, native: await nativeState(), contrast: ratios });
  return ui;
};
const screenshot = async (name) => {
  const path = `test-results/omarchy-${name}.png`;
  await page.screenshot({ path });
  captures.push(path);
};
const captureSurfaces = async (mode) => {
  await screenshot(`system-${mode}-settings`);
  await page.locator(".topbar-new-thread").click();
  const dialog = page.getByRole("dialog", { name: "New thread", exact: true });
  await dialog.waitFor();
  await screenshot(`system-${mode}-dialog`);
  await dialog
    .getByRole("button", { name: "Close dialog", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Open command palette", exact: true })
    .click();
  const palette = page.getByRole("dialog", { name: "Commands", exact: true });
  await palette.waitFor();
  await screenshot(`system-${mode}-palette`);
  await palette
    .getByRole("button", { name: "Close dialog", exact: true })
    .click();
};
try {
  for (let n = 0; !existsSync(join(hubHome, "hub.sock")); n++) {
    assert(
      n < 300 && hub.exitCode === null,
      "Fixture hub did not start: " + hubError,
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  application = await electron.launch({
    args: ["."],
    cwd: resolve("."),
    env,
    colorScheme: null,
    timeout: 60000,
  });
  page = await application.firstWindow();
  page.setDefaultTimeout(15000);
  page.on("pageerror", (error) => errors.push(error.message));
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor({ timeout: 60000 });
  assert.equal(
    (await page.evaluate(() => window.twr.rpc("hub.info"))).pid,
    hub.pid,
  );
  const windows = JSON.parse(
    execFileSync("hyprctl", ["clients", "-j"], { encoding: "utf8" }),
  );
  const window = windows.find(
    (value) => value.pid === application.process().pid,
  );
  assert(window, "Hyprland must identify the real Electron window");
  assert.equal(window.class, "tools.twr.desktop");
  assert.equal(window.xwayland, false, "Omarchy QA must use native Wayland");
  const initial = await readUI();
  assert.equal(initial.preference, "dark");
  assert.equal(initial.scheme, "dark");
  assert.notEqual(initial.background, palettes.dark.background);
  await expect.poll(nativeState).toMatchObject({
    source: "dark",
    dark: true,
    background: initial.background,
  });
  // Discover a light theme through the real file watcher while default Dark stays explicit.
  let changedAt = performance.now();
  writeTheme(palettes.light);
  await expect
    .poll(async () => (await readUI()).loadedBackground, { timeout: 30000 })
    .toBe(palettes.light.background);
  switches.push({
    mode: "light",
    preference: "dark",
    elapsedMs: Math.round(performance.now() - changedAt),
  });
  const stillDark = await readUI();
  assert.equal(stillDark.preference, "dark");
  assert.equal(stillDark.background, initial.background);
  assert.equal(stillDark.scheme, "dark");
  await page
    .locator(".sidebar-footer")
    .getByRole("button", { name: "Settings", exact: true })
    .click();
  await page.getByRole("button", { name: "System", exact: true }).click();
  const light = await verifySystem(palettes.light);
  await captureSurfaces("light");
  changedAt = performance.now();
  writeTheme(palettes.dark);
  const dark = await verifySystem(palettes.dark);
  switches.push({
    mode: "dark",
    preference: "system",
    elapsedMs: Math.round(performance.now() - changedAt),
  });
  await captureSurfaces("dark");
  for (const token of ["overlay", "active", "border-strong", "bubble"])
    assert.notEqual(
      light.tokens[token],
      dark.tokens[token],
      `${token} must follow the active Omarchy palette`,
    );
  await page.getByRole("button", { name: "Dark", exact: true }).click();
  changedAt = performance.now();
  writeTheme(palettes.light);
  await expect
    .poll(async () => (await readUI()).loadedBackground, { timeout: 30000 })
    .toBe(palettes.light.background);
  switches.push({
    mode: "light",
    preference: "dark",
    elapsedMs: Math.round(performance.now() - changedAt),
  });
  const explicitDark = await readUI();
  assert.equal(explicitDark.preference, "dark");
  assert.equal(explicitDark.background, initial.background);
  assert.equal(explicitDark.scheme, "dark");
  await expect.poll(nativeState).toMatchObject({
    source: "dark",
    dark: true,
    background: initial.background,
  });
  await page.getByRole("button", { name: "System", exact: true }).click();
  await verifySystem(palettes.light);
  assert.deepEqual(errors, []);
  const report = {
    date: new Date().toISOString(),
    platform: process.platform,
    nativeWayland: true,
    windowClass: window.class,
    defaultDarkPreserved: true,
    explicitDarkOverridesOmarchy: true,
    systemPreferencePreserved: true,
    switches,
    snapshots,
    captures,
  };
  writeFileSync(
    "test-results/omarchy-qa.json",
    JSON.stringify(report, null, 2) + "\n",
  );
  console.log(
    "PASS: real Linux/Hyprland Wayland, default/explicit Dark override, watched Omarchy light/dark System palettes, native appearance, full surface tokens and readable text contrast",
  );
  console.log(JSON.stringify(report));
} catch (error) {
  if (page && !page.isClosed()) {
    await page
      .screenshot({ path: "test-results/omarchy-failure.png" })
      .catch(() => {});
    console.error(
      "OMARCHY FAILURE",
      JSON.stringify({
        ui: await readUI().catch(() => null),
        native: await nativeState().catch(() => null),
      }),
    );
  }
  throw error;
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
  stopFixture(join(env.TWR_DESKTOP_BINARY_DIR, "twr"), env, {
    workspaces: [],
  });
  rmSync(fixture, { recursive: true, force: true });
}
