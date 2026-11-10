import { MessageReceipts } from "./message-receipts";
import {
  app,
  BrowserWindow,
  ipcMain,
  Menu,
  dialog,
  shell,
  clipboard,
  crashReporter,
  nativeTheme,
  screen,
  systemPreferences,
} from "electron";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
  renameSync,
} from "node:fs";
import { join, resolve, dirname, isAbsolute } from "node:path";
import { homedir } from "node:os";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import * as pty from "node-pty";
import { z } from "zod";
import { Bridge } from "./bridge";
import { SecureStore } from "./secure-store";
import { DesktopTransport } from "./remote-transport";
import { GatewaySetup } from "./gateway-setup";
import {
  coalesceInboxSources,
  localInboxSources,
  WorkspaceListing,
} from "./inbox";
import { localUploadRequest } from "./upload-request";
import {
  cliArgs,
  nonInteractiveArgs,
  externalUrl,
  terminalSchema,
  sizeSchema,
  workspaceSchema,
} from "./validation";
import type { DesktopAppearance, DesktopEvent } from "../shared/types";
import { ENGINE_BINARIES, cleanupEngines, stageEngines } from "./engines";
import { parseLoginEnvironment, runLoginShell } from "./login-env";
import { discoverCatalog } from "./catalog";
import { AppServices } from "./app-services";
import { DesktopVoice } from "./voice";
import { RendererRecovery } from "./crash-recovery";
import { testAutomation } from "./automation";
import { contextMenuTemplate } from "./context-menu";
import {
  MIN_WINDOW,
  restoreWindow,
  windowStateSchema,
  type RestoredWindow,
} from "./window-state";

if (process.platform === "linux" && !app.commandLine.hasSwitch("class"))
  app.commandLine.appendSwitch("class", "tools.twr.desktop");
// Omarchy uses GNOME's secret service under Hyprland, which Chromium does not
// recognize as a keyring desktop. Respect an explicit user-selected backend.
if (
  process.platform === "linux" &&
  !app.commandLine.hasSwitch("password-store") &&
  existsSync(
    join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "omarchy"),
  )
)
  app.commandLine.appendSwitch("password-store", "gnome-libsecret");
// TWR_DESKTOP_TEST_AUTOMATION is honoured by development runs, and by a packaged build only
// when its own bundle carries the QA marker (desktop/automation.ts). Otherwise it is removed
// here, so nothing below, nor the hub or a terminal, sees it.
const automationIgnored =
  process.env.TWR_DESKTOP_TEST_AUTOMATION === "1" &&
  !testAutomation({
    packaged: app.isPackaged,
    env: process.env,
    resourcesPath: process.resourcesPath,
  });
if (automationIgnored) delete process.env.TWR_DESKTOP_TEST_AUTOMATION;
if (app.isPackaged && process.env.TWR_DESKTOP_TEST_AUTOMATION !== "1") {
  app.commandLine.removeSwitch("remote-debugging-port");
  app.commandLine.removeSwitch("remote-debugging-pipe");
  // Chromium's fake microphone and its permission bypass are for tests only.
  app.commandLine.removeSwitch("use-fake-ui-for-media-stream");
  app.commandLine.removeSwitch("use-fake-device-for-media-stream");
  app.commandLine.removeSwitch("use-file-for-fake-audio-capture");
}

let window: BrowserWindow;
let bridge: Bridge | undefined;
let workspace = process.env.TWR_WORKSPACE || "default";
let binaries: { twr: string; bridge: string; gateway: string };
let quitting = false;
let quitRequested = false;
let connectTail: Promise<any> = Promise.resolve();
let loginEnv: NodeJS.ProcessEnv = { ...process.env };
let workspaceReady = false;
let localGeneration = 0;
const exec = promisify(execFile);
let openingTerminals = 0;
let remoteTransport: DesktopTransport;
let services: AppServices | undefined;
let hubVersion: string | undefined;
let voice: DesktopVoice | undefined;
// The microphone is granted only after the user turned voice mode on (`voice-arm`), and only
// for audio, to the app's own page.
let microphoneArmed = false;
const terminals = new Map<
  string,
  {
    process: pty.IPty;
    chunks: string[];
    bytes: number;
    pending: string;
    sequence: number;
    outstanding: number;
    timer?: ReturnType<typeof setTimeout>;
    title: string;
    workspace: string;
    exited: boolean;
  }
>();
const settingsPath = () => join(app.getPath("userData"), "desktop.json");
const appearanceSchema = z
  .object({
    preference: z.enum(["dark", "light", "system"]),
    colorScheme: z.enum(["dark", "light"]),
    background: z
      .string()
      .regex(/^#[0-9a-fA-F]{6}$/)
      .transform((value) => value.toLowerCase()),
  })
  .strict();
let appearance: DesktopAppearance = {
  preference: "dark",
  colorScheme: "dark",
  background: "#0e1013",
};
let desktopSettings: Record<string, unknown> = {};
function saveDesktopSettings(update: Record<string, unknown>) {
  const next = { ...desktopSettings, ...update };
  const path = settingsPath();
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${randomUUID()}.tmp`;
  writeFileSync(temporary, JSON.stringify(next), { mode: 0o600 });
  renameSync(temporary, path);
  desktopSettings = next;
}
function nativeAppearanceSource(value: DesktopAppearance) {
  // Omarchy's selected theme is the system appearance for this client, even
  // when the GTK/portal preference differs. Keep the user's System choice.
  if (value.preference === "system" && process.platform === "linux") {
    const theme = omarchyTheme();
    if (theme)
      return theme.mode === "light" || theme.mode === "dark"
        ? theme.mode
        : value.colorScheme;
  }
  return value.preference;
}
function applyNativeAppearance() {
  const source = nativeAppearanceSource(appearance);
  if (nativeTheme.themeSource !== source) nativeTheme.themeSource = source;
  if (window && !window.isDestroyed())
    window.setBackgroundColor(appearance.background);
}

const devUrl = !app.isPackaged ? process.env.TWR_DESKTOP_DEV_URL : undefined;
if (!app.isPackaged && process.env.TWR_DESKTOP_TEST_USER_DATA)
  app.setPath("userData", resolve(process.env.TWR_DESKTOP_TEST_USER_DATA));
// Crash reports stay on this machine (app.getPath("crashDumps")); nothing is uploaded.
crashReporter.start({ uploadToServer: false });
const appFile = resolve(__dirname, "../dist/index.html");
const env = (): NodeJS.ProcessEnv => ({
  ...loginEnv,
  PATH: [
    ...new Set(
      [
        dirname(binaries.twr),
        ...(loginEnv.PATH || "").split(":"),
        join(homedir(), ".local/bin"),
        join(homedir(), ".cargo/bin"),
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
      ].filter(Boolean),
    ),
  ].join(":"),
});
function emit(event: DesktopEvent) {
  if (window && !window.isDestroyed())
    window.webContents.send("twr:event", event);
}
function trusted(event: Electron.IpcMainInvokeEvent | Electron.IpcMainEvent) {
  if (
    !window ||
    event.sender !== window.webContents ||
    event.senderFrame !== window.webContents.mainFrame
  )
    throw new Error("Untrusted desktop caller");
  const url = new URL(event.senderFrame.url);
  if (
    devUrl
      ? url.origin !== new URL(devUrl).origin
      : url.href !== pathToFileURL(appFile).href
  )
    throw new Error("Untrusted desktop page");
}
// The login environment and the staged engines. The window opens before they are
// ready; every handler that runs a program or uses its PATH waits for them, so
// nothing ever runs with the launch environment by mistake.
let markEnvReady!: () => void;
let failEnv!: (error: unknown) => void;
const envReady = new Promise<void>((resolve, reject) => {
  markEnvReady = resolve;
  failEnv = reject;
});
envReady.catch(() => {});
function handle(
  channel: string,
  callback: (...args: any[]) => any,
  needsEnvironment = false,
) {
  ipcMain.handle(`twr:${channel}`, async (event, ...args) => {
    trusted(event);
    if (needsEnvironment) await envReady;
    return callback(...args);
  });
}
const handleWithEnvironment = (
  channel: string,
  callback: (...args: any[]) => any,
) => handle(channel, callback, true);
function companion() {
  if (!bridge) throw new Error("Connect to a workspace first");
  return bridge;
}
async function findBinaries() {
  const dirs = app.isPackaged
    ? [join(process.resourcesPath, "bin")]
    : ([
        process.env.TWR_DESKTOP_BINARY_DIR,
        resolve(__dirname, "../bin"),
        resolve(__dirname, "../../target/debug"),
        resolve(__dirname, "../../target/release"),
      ].filter(Boolean) as string[]);
  const dir = dirs.find((d) =>
    ENGINE_BINARIES.every((name) => existsSync(join(d, name))),
  );
  if (!dir)
    throw new Error(
      "Bundled twr binaries are missing. In a development checkout run npm run build:rust.",
    );
  // Persistent hubs must never depend on an AppImage mount or macOS app translocation.
  const root = join(app.getPath("userData"), "engines");
  // A packaged build ships its engines' digests; a development checkout's
  // binaries change under it, so they are always hashed.
  const stable = await stageEngines(dir, root, app.isPackaged);
  setTimeout(
    () =>
      void cleanupEngines(
        root,
        stable,
        loginEnv.XDG_CONFIG_HOME
          ? [join(loginEnv.XDG_CONFIG_HOME, "systemd/user")]
          : [],
      ).catch((error: unknown) => {
        const detail = error as { code?: unknown; syscall?: unknown };
        const safe = (value: unknown) =>
          typeof value === "string" && /^[A-Za-z0-9_]{1,32}$/.test(value)
            ? value
            : "unknown";
        console.warn("Engine cleanup retained cached versions", {
          code: safe(detail?.code),
          syscall: safe(detail?.syscall),
        });
      }),
    30000,
  ).unref();
  return {
    twr: join(stable, "twr"),
    bridge: join(stable, "twr-desktop-bridge"),
    gateway: join(stable, "twr-mobile-gateway"),
  };
}
async function loadLoginEnvironment() {
  if (!app.isPackaged && process.env.TWR_DESKTOP_TEST_USER_DATA) return;
  const program = process.env.SHELL || "/bin/sh";
  if (!isAbsolute(program) || !existsSync(program)) {
    services?.log.warn(
      "environment",
      `Login shell ${program} was not found; using the launch environment`,
    );
    return;
  }
  const started = Date.now();
  try {
    const values = parseLoginEnvironment(
      await runLoginShell(program),
      process.env,
    );
    if (!values) {
      services?.log.warn(
        "environment",
        `${program} printed no environment; using the launch environment`,
      );
      return;
    }
    Object.assign(loginEnv, values);
    services?.log.info(
      "environment",
      `Read the login environment from ${program} in ${Date.now() - started} ms`,
    );
  } catch (error) {
    // A broken login shell does not prevent opening settings or diagnosing it.
    services?.log.warn(
      "environment",
      `Could not read the login environment from ${program}; using the launch environment`,
      error,
    );
  }
}
async function cli(args: unknown) {
  if (!workspaceReady)
    throw new Error(
      "Connect to the selected workspace before running CLI commands",
    );
  const words = nonInteractiveArgs(args);
  const planned = await companion().call("desktop.cli_plan", {
    args: words,
    workspace,
  });
  try {
    const result = await exec(binaries.twr, planned, {
      env: env(),
      timeout: 120_000,
      maxBuffer: 16 * 1024 * 1024,
    });
    return { ...result, code: 0 };
  } catch (error: any) {
    if (typeof error.code !== "number") throw new Error(error.message);
    return {
      stdout: String(error.stdout || ""),
      stderr: String(error.stderr || ""),
      code: error.code,
    };
  }
}
async function connect(name: unknown) {
  const next = workspaceSchema.parse(name);
  const generation = localGeneration;
  // The generation is taken before waiting, so leaving the local hub while the
  // environment loads still cancels this connection.
  const task = connectTail.then(async () => {
    await envReady;
    if (generation !== localGeneration)
      throw new Error("This hub connection was closed");
    return connectWorkspace(next, generation);
  });
  connectTail = task.catch(() => {});
  return task;
}
async function connectWorkspace(next: string, generation: number) {
  workspaceReady = false;
  emit({ type: "connection", connected: false });
  const previous = bridge;
  // Attachments are scoped to the old workspace and keep running in their own terminal tabs.
  const current = new Bridge(binaries.bridge, next, env(), (event) => {
    if (bridge === current) {
      if (event.type === "connection" && event.connected && !workspaceReady)
        return;
      emit(event);
    }
  });
  bridge = current;
  previous?.close();
  const paths = await current.call("desktop.paths");
  try {
    const hubInfo = await current.call("desktop.ensure_hub", {
      binary: binaries.twr,
    });
    if (generation !== localGeneration || bridge !== current) {
      current.close();
      throw new Error("This hub connection was closed");
    }
    workspace = next;
    workspaceReady = true;
    emit({ type: "connection", connected: true });
    void current
      .call("notify.looking", { looking: window?.isFocused() || false })
      .catch(() => {});
    saveDesktopSettings({ workspace: next });
    hubVersion = hubInfo.version;
    return {
      ...paths,
      hubVersion: hubInfo.version,
      versionMismatch: hubInfo.version !== paths.version,
    };
  } catch (error) {
    // Retain the companion for config repair, while blocking workspace-bound commands.
    services?.log.warn("hub", `Could not open workspace ${next}`, error);
    emit({ type: "config-available", error: String(error) });
    throw error;
  }
}
/** The third-party notices every package carries (scripts/notices.mjs), in the browser. */
function openNotices() {
  const path = app.isPackaged
    ? join(process.resourcesPath, "THIRD-PARTY-LICENSES.html")
    : resolve(__dirname, "../dist-notices/THIRD-PARTY-LICENSES.html");
  // Automated QA (scripts/integration.mjs) reads what would have opened.
  if (process.env.TWR_DESKTOP_TEST_AUTOMATION === "1") {
    (globalThis as { __twrOpenedNotices?: object }).__twrOpenedNotices = {
      path,
      exists: existsSync(path),
    };
    return;
  }
  if (!existsSync(path)) {
    dialog.showErrorBox(
      "Third-party licences not found",
      app.isPackaged
        ? `This copy of twr has no ${path}. Reinstall it from the release page.`
        : "Write them with node scripts/notices.mjs in clients/.",
    );
    return;
  }
  void shell.openPath(path).then((error) => {
    if (error) dialog.showErrorBox("Could not open the licences", error);
  });
}
function omarchyTheme(): Record<string, string> | null {
  if (process.platform !== "linux") return null;
  try {
    const text = readFileSync(
      join(homedir(), ".config/omarchy/current/theme/colors.toml"),
      "utf8",
    );
    const colors: Record<string, string> = {};
    for (const line of text.split("\n")) {
      const match = line.match(
        /^\s*(background|foreground|accent|selection|selection_background|muted|dark_background|darker_background|lighter_background|red|green|yellow|blue|magenta|cyan|bright_(?:red|green|yellow|blue|magenta|cyan)|color[0-9]+)\s*=\s*["'](#[a-fA-F0-9]{6})["']/,
      );
      if (match) colors[match[1]] = match[2];
    }
    const mode = text.match(/^\s*mode\s*=\s*["'](light|dark)["']/m);
    if (mode) colors.mode = mode[1];
    return colors.background && colors.foreground ? colors : null;
  } catch {
    return null;
  }
}
function register() {
  services!.register(handle, (channel, callback) =>
    ipcMain.on(`twr:${channel}`, (event, ...args) => {
      try {
        trusted(event);
        callback(...args);
      } catch (error) {
        services?.logRejected(channel, error);
      }
    }),
  );
  const vault = new SecureStore(
    join(app.getPath("userData"), "secure", "connections"),
  );
  const offlineVault = new SecureStore(
    join(app.getPath("userData"), "secure", "offline"),
  );
  remoteTransport = new DesktopTransport(
    vault,
    (message) => {
      if (window && !window.isDestroyed())
        window.webContents.send("twr:remote-message", { message });
    },
    !app.isPackaged,
  );
  // Created once the engines are staged; every gateway handler waits for them.
  let gatewaySetup: GatewaySetup | undefined;
  const gateway = () =>
    (gatewaySetup ||= new GatewaySetup(
      binaries.gateway,
      env,
      join(app.getPath("userData"), "gateway.log"),
      (url, timeout) => remoteTransport.check(url, timeout),
      (settings) => saveDesktopSettings({ gateway: settings }),
      desktopSettings.gateway,
    ));
  const messageReceipts = new MessageReceipts(
    new SecureStore(join(app.getPath("userData"), "secure", "receipts")),
  );
  handle("send-message", (input) => {
    const message = z
      .object({
        id: z.uuid(),
        workspace: z.string().min(1).max(100),
        method: z.enum(["say", "thread.send"]),
        params: z.record(z.string(), z.unknown()),
        createdAt: z.number().finite(),
      })
      .parse(input);
    if (!workspaceReady || message.workspace !== workspace)
      throw new Error(
        "Connect to this message’s workspace before delivering it",
      );
    if (JSON.stringify(message.params).length > 2 * 1024 * 1024)
      throw new Error("Message exceeds 2 MiB");
    const currentBridge = companion();
    return messageReceipts.send(message, () =>
      currentBridge.call(message.method, message.params),
    );
  });
  handle("secure-read", (key) =>
    offlineVault.read(z.string().max(512).parse(key)),
  );
  handle("secure-write", (key, value) =>
    offlineVault.write(
      z.string().max(512).parse(key),
      z
        .string()
        .max(8 * 1024 * 1024)
        .parse(value),
    ),
  );
  handle("secure-delete", (key) =>
    offlineVault.delete(z.string().max(512).parse(key)),
  );
  const workspaceListing = new WorkspaceListing(
    async (timeout) =>
      JSON.parse(
        (
          await exec(binaries.twr, ["ws", "list", "--json"], {
            env: env(),
            timeout,
            maxBuffer: 1024 * 1024,
          })
        ).stdout,
      ),
    5000,
  );
  handleWithEnvironment(
    "inbox-sources",
    // One refresh at a time: a request during a refresh shares its result.
    // The listing has its own 5 s budget; the summaries then get 4 s.
    coalesceInboxSources(async () =>
      localInboxSources(
        await workspaceListing.get(),
        (name) =>
          new Bridge(
            binaries.bridge,
            name,
            { ...env(), TWR_BRIDGE_READ_ONLY: "1" },
            () => {},
          ),
        4000,
      ),
    ),
  );
  handle("remote-list", () => remoteTransport.listHubs());
  handle("remote-pair", (request) => remoteTransport.pair(request));
  handle("remote-connect", (request) => remoteTransport.connect(request));
  handle("remote-send", (request) => remoteTransport.send(request));
  handle("remote-disconnect", () => remoteTransport.disconnect());
  handle("remote-remove", (request) => remoteTransport.removeHub(request));
  handle("remote-reset", () => remoteTransport.resetSavedHubs());
  handleWithEnvironment("gateway-status", () => gateway().status());
  handleWithEnvironment("gateway-cancel-invitation", () =>
    gateway().cancelInvitation(),
  );
  handleWithEnvironment("gateway-stop", () => gateway().stop());
  handleWithEnvironment("gateway-settings", () => gateway().saved());
  handle("gateway-check", (request) => remoteTransport.check(request.url));
  handleWithEnvironment("gateway-setup", (request) => gateway().setup(request));
  handleWithEnvironment("gateway-restart", (request) =>
    gateway().restart(request),
  );
  handleWithEnvironment("gateway-invite", (request) =>
    gateway().invite(request),
  );
  handle("deactivate-local", async () => {
    ++localGeneration;
    workspaceReady = false;
    void bridge?.call("notify.looking", { looking: false }).catch(() => {});
    bridge?.close();
    bridge = undefined;
    // Native PTYs remain owned by the app and are restored on returning locally.
    // Switching a hub never discards a running shell, editor, or build.
    for (const session of terminals.values()) {
      session.outstanding = 0;
      if (!session.exited) {
        try {
          session.process.resume();
        } catch {
          /* Exit notification may still be queued. */
        }
      }
    }
  });
  handle("set-appearance", (value) => {
    const next = appearanceSchema.parse(value);
    appearance = next;
    applyNativeAppearance();
    if (JSON.stringify(desktopSettings.appearance) !== JSON.stringify(next))
      saveDesktopSettings({ appearance: next });
  });
  handle("copy-text", (text) =>
    clipboard.writeText(
      z
        .string()
        .max(4 * 1024 * 1024)
        .parse(text),
    ),
  );
  handle("quit", () => {
    app.quit();
  });
  voice = new DesktopVoice(() => binaries.twr, env, emit);
  handleWithEnvironment("voice-status", () => voice!.status());
  handleWithEnvironment("voice-setup", () => voice!.setup());
  handle("voice-arm", async (on) => {
    microphoneArmed = z.boolean().parse(on);
    // Voice QA (scripts/voice-qa.mjs) plays a recording as the microphone in a development
    // build: no system access is asked for. A packaged build always asks.
    const voiceQa =
      !app.isPackaged && process.env.TWR_DESKTOP_TEST_AUTOMATION === "1";
    if (microphoneArmed && process.platform === "darwin" && !voiceQa) {
      // macOS asks once; after that its answer stands until changed in System Settings.
      const access = systemPreferences.getMediaAccessStatus("microphone");
      const granted =
        access === "granted" ||
        (access === "not-determined" &&
          (await systemPreferences.askForMediaAccess("microphone")));
      if (!granted) {
        microphoneArmed = false;
        throw new Error(
          "Microphone access is off for twr. Turn it on in System Settings > Privacy & Security > Microphone.",
        );
      }
    }
  });
  handleWithEnvironment("voice-transcribe", (recording, hint) =>
    voice!.transcribe(recording, hint),
  );
  handleWithEnvironment("voice-speaker", (op) => voice!.speakerControl(op));
  let catalog: ReturnType<typeof discoverCatalog> | undefined;
  handleWithEnvironment(
    "catalog",
    () =>
      (catalog ||= discoverCatalog(
        async (args) =>
          (
            await exec(binaries.twr, args, {
              env: env(),
              timeout: 15000,
              maxBuffer: 1024 * 1024,
            })
          ).stdout,
      )
        .then((items) =>
          items.filter(
            (item) =>
              !["update", "mcp"].includes(item.cli?.[0] || "") &&
              !["hub", "voice transcribe"].includes(item.cli?.join(" ") || ""),
          ),
        )
        .catch((error) => {
          catalog = undefined;
          throw error;
        })),
  );
  handle("bootstrap", () => ({
    workspace,
    hostId: "local",
    platform: process.platform,
    home: homedir(),
    version: app.getVersion(),
    theme: omarchyTheme(),
    terminals: [...terminals].map(([id, t]) => ({
      id,
      title: t.title,
      workspace: t.workspace,
      exited: t.exited,
    })),
  }));
  handle("connect", connect);
  handle(
    "upload-request",
    localUploadRequest(() => ({
      workspace,
      ready: workspaceReady,
      bridge: companion(),
    })),
  );
  handle("rpc", (method, params) => {
    z.string().max(100).parse(method);
    if (method === "desktop.check_updates") return services!.updater.check();
    if (method === "desktop.runner.use")
      return companion().call(method, { binary: binaries.twr });
    if (
      method.startsWith("desktop.") &&
      ![
        "desktop.subscribe",
        "desktop.preferences",
        "desktop.runner.use",
        "desktop.runner.cancel",
        "desktop.runner.status",
        "desktop.retry_attention",
        "desktop.parse_slash",
        "desktop.slash_commands",
      ].includes(method)
    )
      throw new Error("Private desktop operation");
    if (!workspaceReady)
      throw new Error(
        "Connect to the workspace before running hub operations. Configuration can still be repaired in Settings.",
      );
    if (JSON.stringify(params).length > 2 * 1024 * 1024)
      throw new Error("Request too large");
    return companion().call(method, params);
  });
  handleWithEnvironment("cli", cli);
  handle("config-read", () => companion().call("desktop.config.read"));
  handle("config-save", (content, hash) =>
    companion().call("desktop.config.save", {
      content: z
        .string()
        .max(2 * 1024 * 1024)
        .parse(content),
      hash: z
        .string()
        .regex(/^[a-f0-9]{64}$/)
        .parse(hash),
    }),
  );
  handle("config-set", (table, key, value) =>
    companion().call("desktop.config.set", {
      table: z.array(z.string().min(1).max(64)).max(8).parse(table),
      key: z.string().min(1).max(64).parse(key),
      value: z
        .union([z.string().max(4096), z.number().int(), z.boolean()])
        .parse(value),
    }),
  );
  handle("pick-directory", async () => {
    const result = await dialog.showOpenDialog(window, {
      properties: ["openDirectory"],
    });
    return result.canceled ? null : result.filePaths[0];
  });
  handle("open-external", (url) => shell.openExternal(externalUrl(url)));
  handle("reveal", (path) => {
    const p = z.string().max(4096).parse(path);
    if (!isAbsolute(p)) throw new Error("An absolute path is required");
    shell.showItemInFolder(p);
  });
  handle("export", async (name, content) => {
    const text = z
      .string()
      .max(32 * 1024 * 1024)
      .parse(content);
    const result = await dialog.showSaveDialog(window, {
      defaultPath: z
        .string()
        .regex(/^[a-zA-Z0-9_. -]{1,120}$/)
        .parse(name),
    });
    if (!result.canceled && result.filePath)
      writeFileSync(result.filePath, text, { mode: 0o600 });
  });
  handleWithEnvironment("terminal-open", async (request) => {
    if (!workspaceReady)
      throw new Error("Connect to the workspace before starting a terminal");
    const terminalWorkspace = workspace;
    const terminalBridge = companion();
    const value = terminalSchema.parse(request);
    if (
      [...terminals.values()].filter((session) => !session.exited).length +
        openingTerminals >=
      8
    )
      throw new Error(
        "Close a terminal tab before opening another (maximum 8)",
      );
    const cwd = value.cwd || homedir();
    if (!isAbsolute(cwd))
      throw new Error("An absolute working directory is required");
    const command = value.shell ? process.env.SHELL || "/bin/sh" : binaries.twr;
    const words = value.args?.length ? cliArgs(value.args) : [];
    openingTerminals++;
    try {
      const args = value.shell
        ? ["-l"]
        : await terminalBridge.call("desktop.cli_plan", {
            args: words,
            workspace: terminalWorkspace,
          });
      if (!value.shell) {
        try {
          await exec(binaries.twr, ["--desktop-validate", ...args], {
            env: env(),
            cwd,
            timeout: 15000,
            maxBuffer: 1024 * 1024,
          });
        } catch (error: any) {
          throw new Error(error.stderr || error.message);
        }
      }
      const terminal = pty.spawn(command, args, {
        name: "xterm-256color",
        cols: 100,
        rows: 24,
        cwd,
        env: {
          ...env(),
          TERM: "xterm-256color",
          COLORTERM: "truecolor",
        } as Record<string, string>,
      });
      const id = randomUUID();
      const session: ReturnType<typeof terminals.get> & {} = {
        process: terminal,
        chunks: [],
        bytes: 0,
        pending: "",
        sequence: 0,
        outstanding: 0,
        title:
          value.title ||
          (value.shell
            ? "Shell"
            : value.args?.slice(0, 2).join(" ") || "twr CLI"),
        workspace: terminalWorkspace,
        exited: false,
      };
      terminals.set(id, session);
      const flush = () => {
        if (!session.pending) return;
        const data = session.pending;
        session.pending = "";
        session.sequence++;
        if (!workspaceReady) {
          session.outstanding = 0;
          return;
        }
        session.outstanding += data.length;
        emit({ type: "terminal-data", id, data, sequence: session.sequence });
        if (session.outstanding > 512 * 1024) session.process.pause();
      };
      terminal.onData((data) => {
        session.chunks.push(data);
        session.bytes += Buffer.byteLength(data);
        session.pending += data;
        while (session.bytes > 2 * 1024 * 1024 && session.chunks.length > 1) {
          const removed = session.chunks.shift()!;
          session.bytes -= Buffer.byteLength(removed);
        }
        if (!session.timer)
          session.timer = setTimeout(() => {
            session.timer = undefined;
            flush();
          }, 16);
        if (session.pending.length > 128 * 1024) {
          if (session.timer) clearTimeout(session.timer);
          session.timer = undefined;
          flush();
        }
      });
      terminal.onExit(({ exitCode }) => {
        flush();
        session.exited = true;
        emit({ type: "terminal-exit", id, exitCode });
      });
      return { id, title: session.title, workspace: terminalWorkspace };
    } finally {
      openingTerminals--;
    }
  });
  handle("terminal-read", (id) => {
    const session = terminals.get(z.string().uuid().parse(id));
    if (!session) return { data: "", sequence: 0 };
    // The snapshot includes unflushed output, so publish it before choosing the cursor.
    if (session.pending) {
      const data = session.pending;
      session.pending = "";
      emit({ type: "terminal-data", id, data, sequence: ++session.sequence });
    }
    let data = session.chunks.join("");
    if (session.bytes >= 2 * 1024 * 1024) {
      const boundary = data.indexOf("\n");
      if (boundary >= 0) data = "\x1b[0m" + data.slice(boundary + 1);
    }
    session.outstanding = 0;
    if (!session.exited) session.process.resume();
    return { data, sequence: session.sequence, exited: session.exited };
  });
  handle("terminal-close", async (id) => {
    const key = z.string().uuid().parse(id);
    const session = terminals.get(key);
    if (!session) return;
    if (session.timer) clearTimeout(session.timer);
    if (!session.exited) session.process.kill();
    terminals.delete(key);
  });
  ipcMain.on("twr:terminal-focus", (event, focused) => {
    try {
      trusted(event);
      window.webContents.setIgnoreMenuShortcuts(z.boolean().parse(focused));
    } catch {}
  });
  ipcMain.on("twr:terminal-ack", (event, id, count) => {
    try {
      trusted(event);
      const session = terminals.get(z.string().uuid().parse(id));
      if (session) {
        session.outstanding = Math.max(
          0,
          session.outstanding -
            z
              .number()
              .int()
              .min(0)
              .max(4 * 1024 * 1024)
              .parse(count),
        );
        if (session.outstanding < 128 * 1024 && !session.exited)
          session.process.resume();
      }
    } catch {}
  });
  ipcMain.on("twr:terminal-write", (event, id, data) => {
    try {
      trusted(event);
      const session = terminals.get(z.string().uuid().parse(id));
      const text = z
        .string()
        .max(1024 * 1024)
        .parse(data);
      if (session && !session.exited) session.process.write(text);
    } catch {}
  });
  ipcMain.on("twr:terminal-resize", (event, id, cols, rows) => {
    try {
      trusted(event);
      const size = sizeSchema.parse({ cols, rows });
      terminals
        .get(z.string().uuid().parse(id))
        ?.process.resize(size.cols, size.rows);
    } catch {}
  });
}
function showWindow() {
  // During a quit (including an update restart) a relaunch hands over to this
  // process; never reopen a window that is closing.
  if (quitRequested) return;
  if (!window || window.isDestroyed()) {
    void createWindow();
    return;
  }
  if (window.isMinimized()) window.restore();
  window.show();
  window.focus();
}
const rendererRecovery = new RendererRecovery();
let reloadRequested = false;
let crashDialogOpen = false;
function loadApp(target: BrowserWindow) {
  return devUrl ? target.loadURL(devUrl) : target.loadFile(appFile);
}
async function recoverRenderer(target: BrowserWindow, reason: string) {
  const requested = reloadRequested;
  reloadRequested = false;
  const action = rendererRecovery.gone(reason, {
    quitting: quitting || quitRequested || target.isDestroyed(),
    requested,
  });
  if (action === "ignore") return;
  const reload = () => {
    if (!target.isDestroyed())
      loadApp(target).catch((error) =>
        services?.log.error("renderer", "Could not reload the window", error),
      );
  };
  if (action === "reload") {
    services?.log.warn("renderer", `Reloading the window (${reason})`);
    reload();
    return;
  }
  if (crashDialogOpen) return;
  crashDialogOpen = true;
  try {
    services?.log.error(
      "renderer",
      `The window ended repeatedly (${reason}); asking before reloading`,
    );
    // A question on a hidden background window would never be seen.
    if (!target.isVisible()) target.show();
    const { response } = await dialog.showMessageBox(target, {
      type: "error",
      message: "twr's window keeps stopping",
      detail: `The window ended unexpectedly (${reason}) several times in the last few minutes. The hub, its agents and open terminals keep running. Help › Copy Diagnostics records what happened.`,
      buttons: ["Reload", "Quit twr"],
      defaultId: 0,
      cancelId: 0,
    });
    if (response === 1) app.quit();
    else reload();
  } finally {
    crashDialogOpen = false;
  }
}
/** Restores maximized/full-screen state and saves the window's placement as it changes. */
function rememberPlacement(
  target: BrowserWindow,
  placement: RestoredWindow,
  hidden: boolean,
) {
  const restoreState = () => {
    if (placement.fullscreen) target.setFullScreen(true);
    else if (placement.maximized) target.maximize();
  };
  // maximize() would also show a window that starts hidden.
  if (hidden) target.once("show", restoreState);
  else restoreState();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const save = () => {
    clearTimeout(timer);
    timer = undefined;
    if (target.isDestroyed() || target.isMinimized()) return;
    const state = {
      ...target.getNormalBounds(),
      maximized: target.isMaximized(),
      fullscreen: target.isFullScreen(),
    };
    const parsed = windowStateSchema.safeParse(state);
    if (
      parsed.success &&
      JSON.stringify(parsed.data) !== JSON.stringify(desktopSettings.window)
    )
      try {
        saveDesktopSettings({ window: parsed.data });
      } catch (error) {
        services?.log.warn("window", "Could not save the window size", error);
      }
  };
  const later = () => {
    clearTimeout(timer);
    timer = setTimeout(save, 500);
  };
  for (const name of [
    "resize",
    "move",
    "maximize",
    "unmaximize",
    "enter-full-screen",
    "leave-full-screen",
  ] as const)
    target.on(name as "resize", later);
  target.on("close", save);
}
async function createWindow(hidden = false) {
  quitting = false;
  applyNativeAppearance();
  const primary = screen.getPrimaryDisplay();
  const placement = restoreWindow(desktopSettings.window, [
    primary.workArea,
    ...screen
      .getAllDisplays()
      .filter((display) => display.id !== primary.id)
      .map((display) => display.workArea),
  ]);
  window = new BrowserWindow({
    show: !hidden,
    ...placement.bounds,
    minWidth: MIN_WINDOW.width,
    minHeight: MIN_WINDOW.height,
    title: "twr",
    backgroundColor: appearance.background,
    titleBarStyle: process.platform === "darwin" ? "hiddenInset" : "default",
    webPreferences: {
      preload: join(__dirname, "preload.cjs"),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
      webSecurity: true,
      spellcheck: false,
    },
  });
  window.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
  rememberPlacement(window, placement, hidden);
  window.webContents.on(
    "did-start-navigation",
    (_event, _url, _inPlace, mainFrame) => {
      if (mainFrame) void remoteTransport?.disconnect();
    },
  );
  const current = window;
  // Electron has no default right-click menu; a page that handles the event
  // itself (preventDefault) does not get this one.
  window.webContents.on("context-menu", (_event, params) => {
    const template = contextMenuTemplate(params, {
      copyText: (text) => clipboard.writeText(text),
      replaceMisspelling: (word) =>
        current.webContents.replaceMisspelling(word),
    });
    if (template.length)
      Menu.buildFromTemplate(template).popup({
        window: current,
        x: params.x,
        y: params.y,
      });
  });
  window.webContents.on("render-process-gone", (_event, details) => {
    void remoteTransport?.disconnect();
    void recoverRenderer(current, details.reason);
  });
  let hang: AbortController | undefined;
  window.on("unresponsive", async () => {
    if (hang || current.isDestroyed()) return;
    services?.log.warn("renderer", "The window stopped responding");
    hang = new AbortController();
    const signal = hang.signal;
    try {
      if (!current.isVisible()) current.show();
      const { response } = await dialog.showMessageBox(current, {
        type: "warning",
        message: "twr is not responding",
        detail:
          "You can wait for the window to respond or reload it. The hub, its agents and open terminals keep running.",
        buttons: ["Wait", "Reload"],
        defaultId: 0,
        cancelId: 0,
        signal,
      });
      if (response === 1 && !signal.aborted && !current.isDestroyed()) {
        services?.log.warn("renderer", "Reloading the unresponsive window");
        // A hung renderer cannot reload itself: end it, and the
        // render-process-gone handler loads the page again.
        reloadRequested = true;
        current.webContents.forcefullyCrashRenderer();
      }
    } finally {
      hang = undefined;
    }
  });
  // The dialog closes on its own once the window answers again.
  window.on("responsive", () => hang?.abort());
  window.webContents.on("will-navigate", (event, url) => {
    if (url !== (devUrl || pathToFileURL(appFile).href)) event.preventDefault();
  });
  // Only the microphone, only for audio, only for the app's own main frame, only once the
  // user turned voice mode on. Everything else stays refused.
  const appPage = (url: string) => {
    try {
      return devUrl
        ? new URL(url).origin === new URL(devUrl).origin
        : new URL(url).href === pathToFileURL(appFile).href;
    } catch {
      return false;
    }
  };
  window.webContents.session.setPermissionRequestHandler(
    (contents, permission, callback, details) => {
      const media = details as { mediaTypes?: string[]; isMainFrame?: boolean };
      callback(
        permission === "media" &&
          microphoneArmed &&
          contents === window.webContents &&
          media.isMainFrame !== false &&
          Boolean(media.mediaTypes?.length) &&
          media.mediaTypes!.every((t) => t === "audio") &&
          appPage(contents.getURL()),
      );
    },
  );
  window.webContents.session.setPermissionCheckHandler(
    (contents, permission, _origin, details) =>
      permission === "media" &&
      microphoneArmed &&
      contents === window.webContents &&
      (details as { mediaType?: string }).mediaType === "audio" &&
      appPage(contents.getURL()),
  );
  window.webContents.on("will-prevent-unload", (event) => {
    const choice = dialog.showMessageBoxSync(window, {
      type: "question",
      message: "Discard unsaved editor changes?",
      detail:
        "Your configuration, context, or memory edits have not been saved. Keep the window open to save them.",
      buttons: ["Keep editing", "Discard and close"],
      defaultId: 0,
      cancelId: 0,
    });
    if (choice === 1) event.preventDefault();
    else {
      quitting = false;
      quitRequested = false;
    }
  });
  window.on("focus", () => {
    services?.focused();
    if (workspaceReady)
      void bridge?.call("notify.looking", { looking: true }).catch(() => {});
  });
  window.on("blur", () => {
    if (workspaceReady)
      void bridge?.call("notify.looking", { looking: false }).catch(() => {});
  });
  window.on("close", async (event) => {
    // Background mode: keep the renderer, hub connection and terminals alive.
    if (!quitting && !quitRequested && services?.background) {
      event.preventDefault();
      services.log.info("window", "Hiding the window (background mode)");
      window.hide();
      return;
    }
    if (!quitting && [...terminals.values()].some((t) => !t.exited)) {
      event.preventDefault();
      // Quitting from the tray while hidden: the question must be visible.
      if (!window.isVisible()) window.show();
      const result = await dialog.showMessageBox(window, {
        type: "question",
        message: "Close active terminal sessions?",
        detail:
          "Interactive programs in these terminals will exit. The hub and its other agents will keep running.",
        buttons: ["Keep open", "Close terminals and window"],
        defaultId: 0,
        cancelId: 0,
      });
      if (result.response === 1) {
        quitting = true;
        if (quitRequested) app.quit();
        else window.close();
      } else quitRequested = false;
    }
  });
  window.on("closed", () => {
    services?.closed();
    ++localGeneration;
    workspaceReady = false;
    void remoteTransport.disconnect();
    bridge?.close();
    bridge = undefined;
    for (const session of terminals.values())
      if (!session.exited) session.process.kill();
    terminals.clear();
  });
  await loadApp(window);
}
if (!app.requestSingleInstanceLock()) app.quit();
else {
  app.on("second-instance", () => showWindow());
  app
    .whenReady()
    .then(async () => {
      app.setAppUserModelId("tools.twr.desktop");
      if (process.platform === "linux")
        app.setDesktopName("tools.twr.desktop.desktop");
      try {
        const saved = JSON.parse(readFileSync(settingsPath(), "utf8"));
        if (saved && typeof saved === "object" && !Array.isArray(saved))
          desktopSettings = saved;
      } catch {}
      if (!process.env.TWR_WORKSPACE) {
        const savedWorkspace = workspaceSchema.safeParse(
          desktopSettings.workspace,
        );
        if (savedWorkspace.success) workspace = savedWorkspace.data;
      }
      const savedAppearance = appearanceSchema.safeParse(
        desktopSettings.appearance,
      );
      if (savedAppearance.success) appearance = savedAppearance.data;
      applyNativeAppearance();
      services = new AppServices(
        {
          window: () => (window && !window.isDestroyed() ? window : undefined),
          showWindow,
          emit,
          settings: () => desktopSettings,
          save: saveDesktopSettings,
          prepareQuit: () => {
            quitting = true;
            quitRequested = true;
          },
          cancelQuit: () => {
            quitting = false;
            quitRequested = false;
          },
          activeTerminals: () =>
            [...terminals.values()].filter((t) => !t.exited).length,
          facts: () => ({
            window:
              window && !window.isDestroyed()
                ? { visible: window.isVisible(), focused: window.isFocused() }
                : "closed",
            workspace: workspaceReady ? "connected" : "not connected",
            hubVersion,
            terminals: [...terminals.values()].filter((t) => !t.exited).length,
          }),
        },
        // Automated runs, and any run given its own --user-data-dir, keep their log with
        // that isolated user data instead of the installed app's.
        process.env.TWR_DESKTOP_TEST_AUTOMATION === "1" ||
          app.commandLine.hasSwitch("user-data-dir") ||
          (!app.isPackaged && process.env.TWR_DESKTOP_TEST_USER_DATA)
          ? join(app.getPath("userData"), "logs")
          : app.getPath("logs"),
      );
      services.start();
      if (automationIgnored)
        services.log.warn(
          "app",
          "Ignored TWR_DESKTOP_TEST_AUTOMATION: this build has no QA marker",
        );
      // The login shell and the engine copy run while the window opens.
      void (async () => {
        await loadLoginEnvironment();
        binaries = await findBinaries();
      })().then(
        () => {
          services?.log.info(
            "app",
            `Environment and engines ready ${Math.round(process.uptime() * 1000)} ms after launch`,
          );
          markEnvReady();
        },
        (error) => {
          failEnv(error);
          services?.log.error("app", "Startup failed", error);
          dialog.showErrorBox("Could not open twr", String(error));
          app.quit();
        },
      );
      register();
      if (process.platform === "linux") {
        let previous = JSON.stringify(omarchyTheme());
        const timer = setInterval(() => {
          const theme = omarchyTheme();
          const next = JSON.stringify(theme);
          if (next !== previous) {
            previous = next;
            emit({ type: "theme", theme });
          }
        }, 5000);
        timer.unref();
        app.on("will-quit", () => clearInterval(timer));
      }
      Menu.setApplicationMenu(
        Menu.buildFromTemplate([
          ...(process.platform === "darwin"
            ? [{ role: "appMenu" as const }]
            : []),
          {
            label: "File",
            submenu: [
              {
                label: "New Thread",
                accelerator: "CmdOrCtrl+N",
                click: () => emit({ type: "menu-new" }),
              },
              {
                label: "Command Palette",
                accelerator: "CmdOrCtrl+K",
                click: () => emit({ type: "menu-palette" }),
              },
              { type: "separator" as const },
              { role: "close" as const },
              // macOS has Quit in the app menu; Linux needs it here so a
              // background-mode window can always end the app.
              ...(process.platform === "darwin"
                ? []
                : [{ role: "quit" as const, accelerator: "Ctrl+Q" }]),
            ],
          },
          { role: "editMenu" },
          {
            label: "View",
            submenu: [
              { role: "reload" },
              { role: "togglefullscreen" },
              { role: "resetZoom" },
              { role: "zoomIn" },
              { role: "zoomOut" },
              ...(!app.isPackaged ? [{ role: "toggleDevTools" as const }] : []),
            ],
          },
          {
            label: "Help",
            submenu: [
              {
                label: "Check for Updates…",
                click: () => {
                  showWindow();
                  void services?.updater.check();
                  emit({ type: "menu-settings" });
                },
              },
              {
                label: "Copy Diagnostics",
                click: () => {
                  if (services) clipboard.writeText(services.diagnostics());
                },
              },
              {
                label: "Open Log Folder",
                click: () => {
                  if (services) void shell.openPath(services.log.directory);
                },
              },
              { type: "separator" as const },
              {
                label: "twr Documentation",
                click: () => {
                  void shell.openExternal(
                    "https://github.com/ignaval/twr#readme",
                  );
                },
              },
              {
                label: "Third-Party Licences",
                click: () => openNotices(),
              },
            ],
          },
        ]),
      );
      // Packaged QA closes the window the way the title-bar button does.
      // Synthetic input cannot reach native menu accelerators.
      if (process.env.TWR_DESKTOP_TEST_AUTOMATION === "1")
        process.on("SIGUSR2", () => {
          services?.log.info("test", "Closing the window (SIGUSR2)");
          if (window && !window.isDestroyed()) window.close();
        });
      await createWindow(services.startHidden);
      services.log.info(
        "app",
        `Window loaded ${Math.round(process.uptime() * 1000)} ms after launch`,
      );
      void services
        .probe()
        .catch((error) =>
          services?.log.warn("app", "Startup checks failed", error),
        );
      app.on("activate", () => {
        services?.log.info("window", "Activated; showing the window");
        showWindow();
      });
    })
    .catch((error) => {
      services?.log.error("app", "Startup failed", error);
      dialog.showErrorBox("Could not open twr", String(error));
      app.quit();
    });
  app.on("window-all-closed", () => {
    if (process.platform !== "darwin") app.quit();
  });
  app.on("before-quit", () => {
    quitRequested = true;
    voice?.dispose();
  });
}
