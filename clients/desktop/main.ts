import { MessageReceipts } from "./message-receipts";
import {
  app,
  BrowserWindow,
  ipcMain,
  Menu,
  dialog,
  shell,
  clipboard,
  nativeTheme,
  systemPreferences,
} from "electron";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
  copyFileSync,
  chmodSync,
  renameSync,
  statSync,
} from "node:fs";
import { join, resolve, dirname, isAbsolute } from "node:path";
import { homedir } from "node:os";
import { pathToFileURL } from "node:url";
import { randomUUID, createHash } from "node:crypto";
import * as pty from "node-pty";
import { z } from "zod";
import { Bridge } from "./bridge";
import { SecureStore } from "./secure-store";
import { DesktopTransport } from "./remote-transport";
import { GatewaySetup } from "./gateway-setup";
import { coalesceInboxSources, localInboxSources } from "./inbox";
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
import { ENGINE_BINARIES, EngineDigests, cleanupEngines } from "./engines";
import { discoverCatalog } from "./catalog";
import { DesktopVoice } from "./voice";

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
if (app.isPackaged && process.env.TWR_DESKTOP_TEST_AUTOMATION !== "1") {
  app.commandLine.removeSwitch("remote-debugging-port");
  app.commandLine.removeSwitch("remote-debugging-pipe");
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
function handle(channel: string, callback: (...args: any[]) => any) {
  ipcMain.handle(`twr:${channel}`, (event, ...args) => {
    trusted(event);
    return callback(...args);
  });
}
function companion() {
  if (!bridge) throw new Error("Connect to a workspace first");
  return bridge;
}
function findBinaries() {
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
  mkdirSync(root, { recursive: true, mode: 0o700 });
  const cache = new EngineDigests(join(root, "digests.json"));
  const sourceHashes = new Map(
    ENGINE_BINARIES.map((name) => [name, cache.digest(join(dir, name))]),
  );
  const digest = createHash("sha256")
    .update(JSON.stringify([...sourceHashes]))
    .digest("hex");
  const stable = join(app.getPath("userData"), "engines", digest);
  mkdirSync(stable, { recursive: true, mode: 0o700 });
  for (const name of ENGINE_BINARIES) {
    const target = join(stable, name);
    const source = join(dir, name);
    if (
      !existsSync(target) ||
      cache.digest(target) !== sourceHashes.get(name)
    ) {
      const scratch = join(stable, `.${name}-${randomUUID()}`);
      copyFileSync(source, scratch);
      chmodSync(scratch, 0o755);
      renameSync(scratch, target);
      cache.digest(target);
    }
  }
  cache.save();
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
  if (!isAbsolute(program) || !existsSync(program)) return;
  try {
    const { stdout } = await exec(
      program,
      ["-ilc", "printf '\\0TWR_DESKTOP_ENV\\0'; /usr/bin/env -0"],
      { timeout: 8000, maxBuffer: 4 * 1024 * 1024 },
    );
    const marker = "\0TWR_DESKTOP_ENV\0";
    const start = stdout.indexOf(marker);
    if (start < 0) return;
    for (const entry of stdout.slice(start + marker.length).split("\0")) {
      const equals = entry.indexOf("=");
      if (equals <= 0) continue;
      const key = entry.slice(0, equals);
      // Preserve identity and harness controls; never turn an agent-launched process into a user.
      if (
        /^(?:TWR_AGENT$|TWR_ORCH_TOKEN|TWR_TEST_USER_TOKEN_FILE$|TWR_DESKTOP_|ELECTRON_|NODE_OPTIONS$|NODE_PATH$)/.test(
          key,
        )
      )
        continue;
      if (
        ["TWR_HOME", "TWR_WORKSPACE"].includes(key) &&
        process.env[key] !== undefined
      )
        continue;
      loginEnv[key] = entry.slice(equals + 1);
    }
  } catch {
    /* A broken login shell does not prevent opening settings or diagnosing it. */
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
  const task = connectTail.then(() => {
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
    return {
      ...paths,
      hubVersion: hubInfo.version,
      versionMismatch: hubInfo.version !== paths.version,
    };
  } catch (error) {
    // Retain the companion for config repair, while blocking workspace-bound commands.
    emit({ type: "config-available", error: String(error) });
    throw error;
  }
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
  const gateway = new GatewaySetup(
    binaries.gateway,
    env,
    join(app.getPath("userData"), "gateway.log"),
    (url, timeout) => remoteTransport.check(url, timeout),
    (settings) => saveDesktopSettings({ gateway: settings }),
    desktopSettings.gateway,
  );
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
  handle(
    "inbox-sources",
    coalesceInboxSources(async () => {
      const started = Date.now();
      const { stdout } = await exec(binaries.twr, ["ws", "list", "--json"], {
        env: env(),
        timeout: 1000,
        maxBuffer: 1024 * 1024,
      });
      const listing = JSON.parse(stdout);
      return localInboxSources(
        Array.isArray(listing) ? listing : listing.workspaces,
        (name) =>
          new Bridge(
            binaries.bridge,
            name,
            { ...env(), TWR_BRIDGE_READ_ONLY: "1" },
            () => {},
          ),
        Math.max(0, 4000 - (Date.now() - started)),
      );
    }),
  );
  handle("remote-list", () => remoteTransport.listHubs());
  handle("remote-pair", (request) => remoteTransport.pair(request));
  handle("remote-connect", (request) => remoteTransport.connect(request));
  handle("remote-send", (request) => remoteTransport.send(request));
  handle("remote-disconnect", () => remoteTransport.disconnect());
  handle("remote-remove", (request) => remoteTransport.removeHub(request));
  handle("remote-reset", () => remoteTransport.resetSavedHubs());
  handle("gateway-status", () => gateway.status());
  handle("gateway-cancel-invitation", () => gateway.cancelInvitation());
  handle("gateway-stop", () => gateway.stop());
  handle("gateway-settings", () => gateway.saved());
  handle("gateway-check", (request) => remoteTransport.check(request.url));
  handle("gateway-setup", (request) => gateway.setup(request));
  handle("gateway-restart", (request) => gateway.restart(request));
  handle("gateway-invite", (request) => gateway.invite(request));
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
  handle("voice-status", () => voice!.status());
  handle("voice-arm", async (on) => {
    microphoneArmed = z.boolean().parse(on);
    if (microphoneArmed && process.platform === "darwin") {
      const granted = await systemPreferences.askForMediaAccess("microphone");
      if (!granted)
        throw new Error(
          "Microphone access is off for twr. Turn it on in System Settings > Privacy & Security > Microphone.",
        );
    }
  });
  handle("voice-transcribe", (recording, hint) =>
    voice!.transcribe(recording, hint),
  );
  handle("voice-speaker", (op) => voice!.speakerControl(op));
  let catalog: ReturnType<typeof discoverCatalog> | undefined;
  handle(
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
              item.cli?.join(" ") !== "hub",
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
    if (method === "desktop.check_updates") return checkUpdates();
    if (
      method.startsWith("desktop.") &&
      ![
        "desktop.subscribe",
        "desktop.preferences",
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
  handle("cli", cli);
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
  handle("terminal-open", async (request) => {
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
async function checkUpdates() {
  const response = await fetch(
    "https://api.github.com/repos/ignaval/twr/releases/latest",
    {
      headers: { Accept: "application/vnd.github+json" },
      signal: AbortSignal.timeout(15000),
    },
  );
  if (!response.ok)
    throw new Error(
      `Could not check releases (${response.status}). Try again later.`,
    );
  const release = (await response.json()) as any;
  const assets = (release.assets || [])
    .filter((a: any) => /\.dmg$|\.AppImage$|\.pkg\.tar\.zst$/.test(a.name))
    .map((a: any) => ({ name: a.name, url: a.browser_download_url }));
  return {
    installed: app.getVersion(),
    latest: release.tag_name,
    release_page: release.html_url,
    desktop_installers: assets,
    message: assets.length
      ? "Download the desktop installer for your platform. The application and bundled engine must be updated together."
      : "This release does not contain desktop installers yet. The current bundled engine has not been changed.",
  };
}
async function createWindow() {
  quitting = false;
  applyNativeAppearance();
  window = new BrowserWindow({
    width: 1360,
    height: 900,
    minWidth: 420,
    minHeight: 480,
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
  window.webContents.on(
    "did-start-navigation",
    (_event, _url, _inPlace, mainFrame) => {
      if (mainFrame) void remoteTransport?.disconnect();
    },
  );
  window.webContents.on("render-process-gone", () => {
    void remoteTransport?.disconnect();
  });
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
    if (workspaceReady)
      void bridge?.call("notify.looking", { looking: true }).catch(() => {});
  });
  window.on("blur", () => {
    if (workspaceReady)
      void bridge?.call("notify.looking", { looking: false }).catch(() => {});
  });
  window.on("close", async (event) => {
    if (!quitting && [...terminals.values()].some((t) => !t.exited)) {
      event.preventDefault();
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
    ++localGeneration;
    workspaceReady = false;
    void remoteTransport.disconnect();
    bridge?.close();
    bridge = undefined;
    for (const session of terminals.values())
      if (!session.exited) session.process.kill();
    terminals.clear();
  });
  if (devUrl) await window.loadURL(devUrl);
  else await window.loadFile(appFile);
}
if (!app.requestSingleInstanceLock()) app.quit();
else {
  app.on("second-instance", () => {
    if (window) {
      if (window.isMinimized()) window.restore();
      window.focus();
    }
  });
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
      await loadLoginEnvironment();
      binaries = findBinaries();
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
                label: "twr Documentation",
                click: () => {
                  void shell.openExternal(
                    "https://github.com/ignaval/twr#readme",
                  );
                },
              },
            ],
          },
        ]),
      );
      await createWindow();
      app.on("activate", () => {
        if (BrowserWindow.getAllWindows().length === 0) void createWindow();
      });
    })
    .catch((error) => {
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
