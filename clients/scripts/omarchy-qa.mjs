// Run from clients after `npm run build` and `cargo build -p twr-cli -p twr-app` (a debug
// hub: the loop's setup card needs its test reviewer), inside a real Linux Hyprland session:
// node scripts/omarchy-qa.mjs
// All hubs, credentials, settings and Omarchy themes live in a disposable HOME.
//
// It first checks a theme written where Omarchy 3 kept it (~/.config/omarchy/current), the
// Light/Dark/System preferences and native colours. Then it installs Omarchy v4.5.0 into the
// disposable HOME as Omarchy does (its repository at ~/.local/share/omarchy) and switches to
// every stock theme with Omarchy's own `omarchy-theme-set` (headless: no Omarchy shell,
// wallpaper or other apps to retint), which leaves the theme in ~/.local/state/omarchy/current.
// For each theme it follows the switch in the running app, checks that the app applied the
// palette shared/omarchy.ts derives (what tests/omarchy-contrast.test.ts checks), and measures
// the colours the renderer draws on its main screens (scripts/omarchy-audit.mjs). Last come
// broken themes and no theme at all. `TWR_OMARCHY_SOURCE=<dir>` uses a clone of
// basecamp/omarchy at v4.5.0 instead of cloning it; `TWR_OMARCHY_THEMES=<name>,…` visits only
// those stock themes. Writes test-results/omarchy-qa.json,
// test-results/omarchy-themes.json and test-results/omarchy-themes/ (a screenshot of each
// theme, one taken right after each switch, and every measured pair).
import { _electron as electron, expect } from "@playwright/test";
import { stopFixture } from "./fixture-cleanup.mjs";
import { auditScreen, focusRing, shortfalls } from "./omarchy-audit.mjs";
import { spawn, execFileSync, spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  readdirSync,
  renameSync,
  existsSync,
  rmSync,
  symlinkSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
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
writeFileSync(join(repo, "README.md"), "demo\n");
execFileSync(
  "git",
  [
    "-C",
    repo,
    "-c",
    "user.name=twr QA",
    "-c",
    "user.email=qa@example.invalid",
    "commit",
    "-qm",
    "Start",
    "--allow-empty",
  ],
  { stdio: "ignore" },
);
// A reviewer that is never run: the loop's setup card only needs one the hub can offer.
const codex = join(fixture, "codex");
writeFileSync(codex, "#!/bin/sh\nexit 1\n");
chmodSync(codex, 0o755);
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
  TWR_CODEX_BIN: codex,
  TWR_TEST_AGENTS: JSON.stringify([
    { runtime: "codex", found: true, login: "logged_in" },
  ]),
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
// ---------------------------------------------------------------------------------------
// Omarchy v4.5.0's stock themes, switched by Omarchy's own omarchy-theme-set
// ---------------------------------------------------------------------------------------
const OMARCHY = {
  tag: "v4.5.0",
  commit: "ac2690cd7be4f47e4a255f5e2e78d904d436b068",
};
const themeShots = "test-results/omarchy-themes";
const omarchyPath = join(hubHome, ".local/share/omarchy");
const currentColors = join(
  hubHome,
  ".local/state/omarchy/current/theme/colors.toml",
);
const appLog = [];
/** shared/omarchy.ts, tests/omarchy-themes.ts and the thread colours, as the app and the
 * unit tests use them. */
const unitModule = async () => {
  const { build } = await import("esbuild");
  const result = await build({
    stdin: {
      contents:
        'export { omarchyProperties, parseOmarchyColors } from "./shared/omarchy"; export { THREAD_COLORS } from "./shared/threadColor"; export { STOCK_THEMES } from "./tests/omarchy-themes";',
      resolveDir: resolve("."),
      loader: "ts",
    },
    bundle: true,
    format: "esm",
    platform: "node",
    write: false,
    logLevel: "silent",
  });
  return import(
    "data:text/javascript;base64," +
      Buffer.from(result.outputFiles[0].text).toString("base64")
  );
};
/** Omarchy's repository where its installer puts it, at the release its themes come from. */
const installOmarchy = () => {
  let source = process.env.TWR_OMARCHY_SOURCE;
  if (source) source = resolve(source);
  else {
    source = join(fixture, "omarchy");
    execFileSync(
      "git",
      [
        "clone",
        "-q",
        "--depth",
        "1",
        "--branch",
        OMARCHY.tag,
        "https://github.com/basecamp/omarchy.git",
        source,
      ],
      { stdio: "inherit" },
    );
  }
  assert.equal(
    execFileSync("git", ["-C", source, "rev-parse", "HEAD"], {
      encoding: "utf8",
    }).trim(),
    OMARCHY.commit,
    `TWR_OMARCHY_SOURCE must be basecamp/omarchy at ${OMARCHY.tag}`,
  );
  mkdirSync(dirname(omarchyPath), { recursive: true });
  symlinkSync(source, omarchyPath);
  mkdirSync(join(fixture, "run"), { recursive: true, mode: 0o700 });
};
/** `omarchy theme set <name>`, as Omarchy runs it, without its shell and other apps. */
const omarchyThemeSet = (name) => {
  const started = performance.now();
  const result = spawnSync(join(omarchyPath, "bin/omarchy-theme-set"), [name], {
    env: {
      ...process.env,
      HOME: hubHome,
      OMARCHY_PATH: omarchyPath,
      PATH: `${join(omarchyPath, "bin")}:${process.env.PATH}`,
      OMARCHY_THEME_HEADLESS: "1",
      XDG_RUNTIME_DIR: join(fixture, "run"),
    },
    encoding: "utf8",
    timeout: 120000,
  });
  assert.equal(
    result.status,
    0,
    `omarchy-theme-set ${name} failed: ${result.stderr}${result.error ?? ""}`,
  );
  return Math.round(performance.now() - started);
};
/** What the window shows at this instant: the root's palette, text and background. */
const glance = () =>
  page.evaluate(() => {
    const root = document.documentElement;
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const context = canvas.getContext("2d", { willReadFrequently: true });
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
    const style = getComputedStyle(root);
    return {
      omarchy: root.dataset.omarchy,
      theme: root.dataset.theme,
      loaded: root.style.getPropertyValue("--omarchy-bg").trim().toLowerCase(),
      background: hex(style.backgroundColor),
      text: hex(style.color),
    };
  });
/** A screenshot of the whole Hyprland output (the window's frame and menu bar included),
 * when grim is installed. */
const screenGrab = (path) => {
  const result = spawnSync("grim", [path], { timeout: 20000 });
  return result.status === 0 ? path : null;
};
/** The colour at a point of a PNG, through Electron's own decoder. */
const pixelAt = (path, x, y) =>
  application.evaluate(
    ({ nativeImage }, { path, x, y }) => {
      const image = nativeImage.createFromPath(path);
      const { width } = image.getSize();
      const bitmap = image.toBitmap();
      const at = (Math.round(y) * width + Math.round(x)) * 4;
      return (
        "#" +
        [bitmap[at + 2], bitmap[at + 1], bitmap[at]]
          .map((value) => value.toString(16).padStart(2, "0"))
          .join("")
      );
    },
    { path, x, y },
  );
const hyprWindow = () =>
  JSON.parse(
    execFileSync("hyprctl", ["clients", "-j"], { encoding: "utf8" }),
  ).find((value) => value.pid === application.process().pid);
/** The native menu bar's colour: a point of it in a screenshot of the output. */
const menuBarColor = async (grab) => {
  if (!grab) return null;
  const client = hyprWindow();
  const monitor = JSON.parse(
    execFileSync("hyprctl", ["monitors", "-j"], { encoding: "utf8" }),
  ).find((value) => value.id === client.monitor);
  const scale = monitor?.scale || 1;
  const content = await application.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    return {
      bounds: window.getBounds(),
      content: window.getContentBounds(),
      menu: window.isMenuBarVisible(),
    };
  });
  const bar = content.content.y - content.bounds.y;
  if (!content.menu || bar < 8) return { menuBar: false };
  return {
    menuBar: true,
    color: await pixelAt(
      grab,
      (client.at[0] - monitor.x + 4) * scale,
      (client.at[1] - monitor.y + bar / 2) * scale,
    ),
  };
};
const sidebarShown = async () => {
  const show = page.getByRole("button", { name: "Show sidebar", exact: true });
  if (await show.isVisible()) await show.click();
};
const toOrchestrator = async () => {
  await sidebarShown();
  await page
    .locator(".sidebar")
    .getByRole("button", { name: "Orchestrator", exact: true })
    .first()
    .click();
};
const threadItem = (name) =>
  page.locator(".thread-list .thread-item").filter({ hasText: name });
const selectThread = async (name) => {
  await sidebarShown();
  await threadItem(name).locator(".thread-card").click();
  await page
    .getByRole("button", { name: "Show thread details", exact: true })
    .click();
  await page.locator("#twr-inspector").waitFor();
};
const closeInspector = async () => {
  const close = page.getByRole("button", {
    name: "Close thread details",
    exact: true,
  });
  if (await close.isVisible()) await close.click();
};
const navTo = async (name) => {
  await sidebarShown();
  if (name === "Settings")
    return page
      .locator(".sidebar-footer")
      .getByRole("button", { name, exact: true })
      .click();
  await page
    .locator(".sidebar nav")
    .first()
    .getByRole("button", { name })
    .first()
    .click();
};
/** Keyboard focus on `locator`: focus the element before it, then Tab. */
const keyboardFocus = async (locator) => {
  await locator.focus();
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Tab");
};
const rpc = (method, params = {}) =>
  page.evaluate(({ method, params }) => window.twr.rpc(method, params), {
    method,
    params,
  });
/** Threads with a request card, a conversation with code, and a loop's setup card. */
const stockFixture = async () => {
  for (const [name, mode] of [
    ["api", "auto"],
    ["web-ui", "manual"],
    ["release-notes", "manual"],
  ])
    await rpc("thread.new", { project: "demo", name, runtime: "fake", mode });
  await rpc("thread.send", {
    thread: "api",
    prompt: "!write lib.rs pub fn f() { todo!() }\nWrote it.",
  });
  await rpc("thread.send", {
    thread: "web-ui",
    prompt: "!perm Bash npm run test:integration",
  });
  await rpc("thread.send", {
    thread: "release-notes",
    prompt: "## Summary\n\nChangelog and migration notes are ready for review.",
  });
  await rpc("say", {
    to: "orchestrator",
    message:
      "## Search is ready for review\n\nCursor pagination is complete. **Web UI** needs permission to run `npm run test:integration`.\n\n```ts\nawait search({ query, after: cursor, limit: 25 });\n```\n\n> After testing, I can prepare the review.",
  });
  await expect
    .poll(
      async () =>
        (await rpc("attention.list", { all: true })).some(
          (item) => item.reason === "permission",
        ),
      { timeout: 60000 },
    )
    .toBe(true);
  await expect
    .poll(
      async () => {
        const api = await rpc("thread.get", { thread: "api" });
        return api?.status === "done" && Number(api.git?.dirty_files) > 0;
      },
      { timeout: 60000 },
    )
    .toBe(true);
  // Review loop with nothing saved raises the setup card.
  await selectThread("api");
  const loops = page.locator("#twr-inspector .inspector-loops");
  await loops.scrollIntoViewIfNeeded();
  await loops.getByRole("button", { name: "Review loop", exact: true }).click();
  await page
    .locator("#twr-inspector .attention-loop-setup")
    .waitFor({ timeout: 60000 });
  await closeInspector();
};
/** The text pairs of the screens, the field edges, the focus rings and the thread names. */
const auditTheme = async (unit) => {
  const audits = [];
  const focus = [];
  const add = async (screen) => audits.push(await auditScreen(page, screen));
  // The orchestrator's conversation: the thread list, markdown with code, the composer.
  await toOrchestrator();
  await closeInspector();
  await page.locator(".markdown pre").first().waitFor();
  await page.evaluate(() => {
    const scroller = document.querySelector(".conversation-scroll");
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  });
  await add("conversation");
  const composer = page.getByRole("textbox", {
    name: "Message orchestrator",
    exact: true,
  });
  await keyboardFocus(composer);
  await add("composer-focused");
  // An error notice: a message that is only "!" (an empty shell command, never run).
  await composer.fill("!");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  const toast = page.locator('.toast[role="alert"]');
  await toast.waitFor();
  await add("notice");
  await toast
    .getByRole("button", { name: "Dismiss error", exact: true })
    .click();
  await composer.fill("");
  // A disabled control: Send with nothing written (measured, not held to 4.5:1).
  await add("disabled-send");
  await keyboardFocus(
    page.getByRole("button", { name: "New thread", exact: true }).first(),
  );
  focus.push(await focusRing(page, "button-focus"));
  // A thread's inspector with its Loops section, then its conversation with its
  // permission request.
  await selectThread("web-ui");
  await add("inspector");
  await page
    .locator("#twr-inspector .inspector-loops")
    .scrollIntoViewIfNeeded();
  await add("inspector-loops");
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await page
    .locator(".conversation-attention .attention-card")
    .first()
    .waitFor();
  await add("request-card");
  // The composer's edge is its focus ring: for a thread, towards the thread's ink.
  await keyboardFocus(
    page.getByRole("textbox", { name: "Message web-ui directly", exact: true }),
  );
  await add("composer-focused-thread");
  // The setup card in the inspector, and its Change… editor.
  await selectThread("api");
  const setup = page.locator("#twr-inspector .attention-loop-setup");
  await setup.scrollIntoViewIfNeeded();
  await add("setup-card");
  await keyboardFocus(setup.getByRole("button", { name: "Change…" }));
  focus.push(await focusRing(page, "setup-change-focus"));
  await page.keyboard.press("Enter");
  const editor = page.getByRole("dialog", { name: "Loop settings · api" });
  await editor.waitFor();
  await editor
    .getByText("Loading loop settings…")
    .waitFor({ state: "hidden", timeout: 30000 });
  await add("loop-editor");
  const editorField = editor
    .locator("select, input:not([type=checkbox], [type=radio])")
    .first();
  if (await editorField.count()) {
    await keyboardFocus(editorField);
    focus.push(await focusRing(page, "loop-editor-field-focus"));
  }
  await page.keyboard.press("Escape");
  await editor.waitFor({ state: "hidden" });
  await closeInspector();
  // Needs you, Settings, and the New thread dialog.
  await navTo(/^Needs you/);
  await page.locator(".manager .attention-card").first().waitFor();
  await add("needs-you");
  await navTo("Settings");
  await page
    .getByRole("heading", { name: "Appearance", exact: true })
    .waitFor();
  await add("settings");
  const loopSettings = page
    .locator(".settings-section")
    .filter({ has: page.getByRole("heading", { name: "Loops", exact: true }) });
  if (await loopSettings.count()) {
    await loopSettings.first().scrollIntoViewIfNeeded();
    await add("settings-loops");
  }
  await page.locator(".topbar-new-thread").click();
  const dialog = page.getByRole("dialog", { name: "New thread", exact: true });
  await dialog.waitFor();
  await dialog.locator("#field-name").fill("result-filters");
  await add("new-thread-dialog");
  await keyboardFocus(dialog.locator("#field-name"));
  focus.push(await focusRing(page, "dialog-field-focus"));
  await dialog
    .getByRole("button", { name: "Close dialog", exact: true })
    .click();
  // Every thread colour's name, as the renderer resolves it on the app's surfaces.
  const inks = await page.evaluate((colors) => {
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const context = canvas.getContext("2d", { willReadFrequently: true });
    const rgb = (color) => {
      context.clearRect(0, 0, 1, 1);
      context.fillStyle = color;
      context.fillRect(0, 0, 1, 1);
      return [...context.getImageData(0, 0, 1, 1).data].slice(0, 3);
    };
    const lum = (c) =>
      c
        .map((v) => v / 255)
        .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
        .reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
    const ratio = (a, b) =>
      (Math.max(lum(a), lum(b)) + 0.05) / (Math.min(lum(a), lum(b)) + 0.05);
    const host = document.querySelector(".sidebar") || document.body;
    const probe = document.createElement("span");
    host.append(probe);
    const surfaces = {};
    for (const name of [
      "bg",
      "panel",
      "raised",
      "overlay",
      "hover",
      "active",
    ]) {
      probe.style.backgroundColor = `var(--${name})`;
      surfaces[name] = rgb(getComputedStyle(probe).backgroundColor);
    }
    probe.style.backgroundColor = "";
    probe.className = "thread-ink";
    probe.textContent = "thread";
    const out = [];
    for (const color of colors) {
      probe.style.setProperty(
        "--thread-ink-omarchy",
        `var(--omarchy-ink-${color.slice(1)})`,
      );
      const ink = rgb(getComputedStyle(probe).color);
      let worst = { ratio: Infinity };
      for (const [surface, value] of Object.entries(surfaces)) {
        const r = Math.round(ratio(ink, value) * 100) / 100;
        if (r < worst.ratio) worst = { ratio: r, surface };
      }
      out.push({
        color,
        ink: "#" + ink.map((v) => v.toString(16).padStart(2, "0")).join(""),
        ...worst,
      });
    }
    probe.remove();
    return out;
  }, unit.THREAD_COLORS);
  await toOrchestrator();
  const text = audits.flatMap((a) => a.text);
  const fields = audits.flatMap((a) => a.fields);
  return { text, fields, focus, inks };
};
const lowest = (items) =>
  items.reduce(
    (low, item) => (!low || item.ratio < low.ratio ? item : low),
    null,
  );
const stock = [];
const runStockThemes = async () => {
  const unit = await unitModule();
  installOmarchy();
  mkdirSync(themeShots, { recursive: true });
  const names = readdirSync(join(omarchyPath, "themes")).sort();
  assert.deepEqual(
    names,
    Object.keys(unit.STOCK_THEMES).sort(),
    "tests/omarchy-themes.ts must hold every stock theme of Omarchy " +
      OMARCHY.tag,
  );
  // The unit tests' palettes are the release's, key for key.
  for (const name of names) {
    const file = unit.parseOmarchyColors(
      readFileSync(join(omarchyPath, "themes", name, "colors.toml"), "utf8"),
    );
    const fixture = unit.parseOmarchyColors(unit.STOCK_THEMES[name]);
    for (const [key, value] of Object.entries(fixture))
      assert.equal(
        file[key]?.toLowerCase(),
        value.toLowerCase(),
        `${name}: tests/omarchy-themes.ts ${key}`,
      );
  }
  // Light themes after dark ones and back: alphabetical order alternates, and the first
  // theme comes again at the end and must look as it did.
  // TWR_OMARCHY_THEMES=<name>,<name> visits only those (to repeat one quickly).
  const only = process.env.TWR_OMARCHY_THEMES?.split(",").filter(Boolean);
  const visit = only ? names.filter((name) => only.includes(name)) : names;
  assert(visit.length, "TWR_OMARCHY_THEMES names no stock theme");
  const order = [
    ...visit,
    visit.includes("tokyo-night") ? "tokyo-night" : visit[0],
  ];
  let fixtureReady = false;
  for (const [index, name] of order.entries()) {
    const label = `${String(index + 1).padStart(2, "0")}-${name}`;
    const before = await glance();
    const commandMs = omarchyThemeSet(name);
    // Right after the switch, before the app has looked: what a person sees then.
    const switchShot = `${themeShots}/${label}-switch.png`;
    await page.screenshot({ path: switchShot });
    const screenShot = screenGrab(`${themeShots}/${label}-switch-screen.png`);
    const theme = unit.parseOmarchyColors(readFileSync(currentColors, "utf8"));
    const expected = unit.omarchyProperties(theme);
    assert(expected, `${name} must give a palette`);
    const background = expected["--omarchy-bg"];
    // Follow the switch: every state the window passes through stays a whole, readable
    // palette (the old theme or the new one), never the app's own or a mix.
    const seen = [];
    const started = performance.now();
    for (;;) {
      const now = await glance();
      const key = JSON.stringify(now);
      if (!seen.length || JSON.stringify(seen.at(-1).state) !== key)
        seen.push({ at: Math.round(performance.now() - started), state: now });
      if (now.loaded === background && now.background === background) break;
      assert(
        performance.now() - started < 30000,
        `${name}: the app did not follow the switch: ${JSON.stringify(now)}`,
      );
      await new Promise((r) => setTimeout(r, 50));
    }
    const followedMs = Math.round(performance.now() - started) + commandMs;
    for (const { state } of seen) {
      assert.equal(
        state.omarchy,
        "true",
        `${name}: mid-switch state ${JSON.stringify(state)}`,
      );
      assert(
        state.background === before.background ||
          state.background === background,
        `${name}: mid-switch background ${state.background} is neither theme's`,
      );
      assert(
        contrast(state.text, state.background) >= 4.5,
        `${name}: mid-switch text ${state.text} on ${state.background}`,
      );
    }
    // The app applied exactly the palette the unit tests check.
    const applied = await page.evaluate(
      (names) =>
        Object.fromEntries(
          names.map((n) => [
            n,
            document.documentElement.style.getPropertyValue(n).trim(),
          ]),
        ),
      Object.keys(expected),
    );
    assert.deepEqual(applied, expected, `${name}: applied palette`);
    await expect.poll(nativeState, { timeout: 30000 }).toEqual({
      source: theme.mode || expected["--omarchy-scheme"],
      dark: (theme.mode || expected["--omarchy-scheme"]) === "dark",
      background,
    });
    if (!fixtureReady) {
      await stockFixture();
      fixtureReady = true;
    }
    const audit = await auditTheme(unit);
    await toOrchestrator();
    await page.locator(".markdown pre").first().waitFor();
    const shot = `${themeShots}/${label}.png`;
    await page.screenshot({ path: shot });
    const settledGrab = screenGrab(`${themeShots}/${label}-screen.png`);
    // Electron draws the Linux menu bar from the GTK theme the app started with, whatever
    // nativeTheme says (docs/desktop.md › Appearance): recorded, not required to follow.
    const menu = await menuBarColor(settledGrab);
    if (menu?.menuBar) {
      menu.followsTheme =
        luminance(menu.color) < 0.18 ===
        (expected["--omarchy-scheme"] === "dark");
      if (!menu.followsTheme)
        console.log(
          `NOTE ${name}: the native menu bar is ${menu.color} on a ${expected["--omarchy-scheme"]} theme`,
        );
    }
    const short = shortfalls(audit);
    const counted = audit.text.filter((t) => !t.disabled);
    const result = {
      theme: name,
      mode: theme.mode,
      applied,
      background,
      commandMs,
      followedMs,
      states: seen,
      native: await nativeState(),
      menu,
      pairs: counted.length,
      lowestText: lowest(counted),
      lowestLargeText: lowest(counted.filter((t) => t.large)),
      lowestThreadName: lowest(counted.filter((t) => t.threadName)),
      lowestField: lowest(audit.fields.filter((f) => !f.disabled)),
      lowestFocus: lowest(audit.focus.filter(Boolean)),
      lowestInk: lowest(audit.inks),
      disabled: lowest(audit.text.filter((t) => t.disabled)),
      shortfalls: short,
      screenshots: [switchShot, screenShot, shot, settledGrab].filter(Boolean),
    };
    stock.push(result);
    writeFileSync(
      `${themeShots}/${label}.json`,
      JSON.stringify({ ...result, audit }, null, 2) + "\n",
    );
    console.log(
      `${short.length ? "FAIL" : "ok"} ${name}: lowest text ${result.lowestText?.ratio} (${result.lowestText?.screen}: ${result.lowestText?.where}), field ${result.lowestField?.ratio}, focus ${result.lowestFocus?.ratio}, names ${result.lowestInk?.ratio}; followed in ${followedMs} ms`,
    );
  }
  // Nothing stale: the first theme again, after the others, looks as it did.
  const first = stock.find((s) => s.theme === order.at(-1));
  const again = stock.at(-1);
  for (const key of [
    "background",
    "applied",
    "native",
    "lowestText",
    "lowestField",
    "lowestFocus",
    "lowestInk",
  ])
    assert.deepEqual(
      { ...again[key], ...(again[key]?.rect ? { rect: null } : {}) },
      { ...first[key], ...(first[key]?.rect ? { rect: null } : {}) },
      `${again.theme} again: ${key} changed`,
    );
};
// ---------------------------------------------------------------------------------------
// Broken themes, and none: the app stays readable and says nothing alarming
// ---------------------------------------------------------------------------------------
const hostile = [];
const runHostileThemes = async () => {
  const userThemes = join(hubHome, ".config/omarchy/themes");
  const cases = [
    {
      name: "twr-qa-same-colours",
      files: {
        "colors.toml":
          'mode = "dark"\nbackground = "#262626"\nforeground = "#262626"\naccent = "#262626"\n',
      },
      omarchy: "true",
    },
    {
      name: "twr-qa-missing-keys",
      files: {
        "colors.toml": 'background = "#f4f1ea"\nforeground = "#6b6b6b"\n',
      },
      omarchy: "true",
    },
    {
      name: "twr-qa-invalid-colours",
      files: {
        "colors.toml":
          'mode = "dark"\nbackground = "blue"\nforeground = "rgb(255, 255, 255)"\naccent = "#12"\nred = "#gggggg"\n',
      },
      omarchy: "false",
    },
    {
      name: "twr-qa-no-colours",
      files: { "README.md": "A theme folder without colors.toml.\n" },
      omarchy: "false",
    },
  ];
  // Back to a stock theme first, so each case starts from a theme the app follows.
  for (const item of [...cases, { name: "none", omarchy: "false" }]) {
    const logStart = appLog.length;
    const errorStart = errors.length;
    omarchyThemeSet("tokyo-night");
    await expect
      .poll(async () => (await glance()).loaded, { timeout: 30000 })
      .toBe("#1a1b26");
    await expect.poll(async () => (await glance()).omarchy).toBe("true");
    if (item.name === "none") {
      // No Omarchy at all any more: neither place has a theme.
      rmSync(join(hubHome, ".local/state/omarchy"), {
        recursive: true,
        force: true,
      });
      rmSync(join(hubHome, ".config/omarchy/current"), {
        recursive: true,
        force: true,
      });
    } else {
      const directory = join(userThemes, item.name);
      mkdirSync(directory, { recursive: true });
      for (const [file, text] of Object.entries(item.files))
        writeFileSync(join(directory, file), text);
      omarchyThemeSet(item.name);
    }
    await expect
      .poll(async () => (await glance()).omarchy, { timeout: 30000 })
      .toBe(item.omarchy);
    if (item.omarchy === "true")
      await expect
        .poll(async () => (await glance()).loaded, { timeout: 30000 })
        .not.toBe("#1a1b26");
    // The app's own colours for System follow the desktop's scheme, which the native side
    // learns after the renderer: the window settles on them a moment later.
    if (item.omarchy === "false")
      await expect
        .poll(
          async () =>
            (await nativeState()).background === (await glance()).background,
          {
            timeout: 15000,
            message: `${item.name}: the native window follows the app's own colours`,
          },
        )
        .toBe(true);
    const now = await glance();
    const native = await nativeState();
    const audit = await auditTheme(await unitModule());
    // The app's own palette (what an unusable theme falls back to) holds text and focus
    // rings to the same ratios; its fields' edges are its own design, not held to 3:1 as
    // an Omarchy palette's are (docs/desktop.md): they are recorded.
    const short = shortfalls(audit).filter(
      (s) => item.omarchy === "true" || s.type !== "field",
    );
    const label = `hostile-${item.name}`;
    await page.screenshot({ path: `${themeShots}/${label}.png` });
    const result = {
      theme: item.name,
      files: item.files ?? null,
      state: now,
      native,
      lowestText: lowest(audit.text.filter((t) => !t.disabled)),
      lowestField: lowest(audit.fields.filter((f) => !f.disabled)),
      lowestFocus: lowest(audit.focus.filter(Boolean)),
      shortfalls: short,
      appLog: appLog.slice(logStart),
      pageErrors: errors.slice(errorStart),
    };
    hostile.push(result);
    writeFileSync(
      `${themeShots}/${label}.json`,
      JSON.stringify({ ...result, audit }, null, 2) + "\n",
    );
    console.log(
      `${short.length ? "FAIL" : "ok"} ${item.name}: data-omarchy=${now.omarchy}, text ${now.text} on ${now.background}, lowest text ${result.lowestText?.ratio}, ${result.appLog.length} log lines`,
    );
  }
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
  for (const stream of [
    application.process().stdout,
    application.process().stderr,
  ])
    stream?.on("data", (data) =>
      appLog.push(...String(data).split("\n").filter(Boolean)),
    );
  page = await application.firstWindow();
  page.setDefaultTimeout(15000);
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (["error", "warning"].includes(message.type()))
      appLog.push(`console.${message.type()}: ${message.text()}`);
  });
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
  await runStockThemes();
  await runHostileThemes();
  assert.deepEqual(errors, []);
  const failing = [...stock, ...hostile].filter((t) => t.shortfalls.length);
  writeFileSync(
    "test-results/omarchy-themes.json",
    JSON.stringify(
      {
        date: new Date().toISOString(),
        omarchy: OMARCHY,
        hyprland: execFileSync("hyprctl", ["version", "-j"], {
          encoding: "utf8",
        }),
        electron: await application.evaluate(() => process.versions.electron),
        themes: stock,
        hostile,
      },
      null,
      2,
    ) + "\n",
  );
  assert.deepEqual(
    failing.map((t) => ({ theme: t.theme, shortfalls: t.shortfalls })),
    [],
    "Every theme's text reads at 4.5:1 (3:1 large), its field edges and focus rings at 3:1",
  );
  for (const theme of stock)
    assert(
      theme.lowestInk.ratio >= 4.5,
      `${theme.theme}: a thread name reads at ${theme.lowestInk.ratio}:1 (${theme.lowestInk.color} on ${theme.lowestInk.surface})`,
    );
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
    stockThemes: stock.map((t) => ({
      theme: t.theme,
      lowestText: t.lowestText?.ratio,
      lowestField: t.lowestField?.ratio,
      lowestFocus: t.lowestFocus?.ratio,
      lowestThreadName: t.lowestInk?.ratio,
      followedMs: t.followedMs,
    })),
  };
  writeFileSync(
    "test-results/omarchy-qa.json",
    JSON.stringify(report, null, 2) + "\n",
  );
  console.log(
    `PASS: real Linux/Hyprland Wayland, default/explicit Dark override, watched Omarchy light/dark System palettes, native appearance, full surface tokens and readable text contrast; Omarchy ${OMARCHY.tag}'s ${stock.length - 1} stock themes switched by omarchy-theme-set, measured on every screen; broken and missing themes fall back readably`,
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
