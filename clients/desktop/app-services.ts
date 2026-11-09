import { app, dialog, shell, type BrowserWindow } from "electron";
import { execFile } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir, release } from "node:os";
import { join, resolve } from "node:path";
import { z } from "zod";
import {
  AUTOSTART_ARG,
  autostartPath,
  linuxAutostartEnabled,
  linuxLaunchCommand,
  setLinuxAutostart,
  refreshLinuxAutostart,
} from "./autostart";
import {
  DiagnosticLog,
  RateLimiter,
  countCrashReports,
  diagnosticsReport,
} from "./diagnostics";
import { Notifier, type Post } from "./notifier";
import { AppTray, statusNotifierAvailable } from "./tray";
import { attentionSummarySchema } from "./validation";
import { WORKSPACE_REFRESH_MS } from "../shared/workspaceTree";
import {
  FIRST_UPDATE_CHECK_DELAY,
  UPDATE_CHECK_INTERVAL,
  detectInstallKind,
  developerIdSigned,
} from "./update-policy";
import { Updater } from "./updater";
import type {
  AttentionSummary,
  DesktopAppSettings,
  DesktopEvent,
} from "../shared/types";

const errorSchema = z
  .object({
    message: z.string().max(4000),
    stack: z.string().max(16000).optional(),
    source: z.string().max(200).optional(),
  })
  .strict();

export interface AppServiceHost {
  window(): BrowserWindow | undefined;
  showWindow(): void;
  emit(event: DesktopEvent): void;
  settings(): Record<string, unknown>;
  save(update: Record<string, unknown>): void;
  /** Ends the session without the background-close behavior. */
  prepareQuit(): void;
  /** Undoes prepareQuit when the quit does not happen. */
  cancelQuit(): void;
  activeTerminals(): number;
  facts(): Record<string, unknown>;
}

export class AppServices {
  readonly log: DiagnosticLog;
  readonly kind = detectInstallKind({
    platform: process.platform,
    packaged: app.isPackaged,
    env: process.env,
    execPath: process.execPath,
    packageType: readPackageType(),
  });
  updater!: Updater;
  private tray: AppTray;
  private notifier: Notifier;
  private trayAvailable = process.platform !== "linux";
  private errors = new RateLimiter(30, 60_000);
  private rejected = new RateLimiter(10, 60_000);
  private timer?: ReturnType<typeof setInterval>;
  private poll?: ReturnType<typeof setInterval>;
  private autostartFile = autostartPath(process.env, homedir());
  constructor(
    private readonly host: AppServiceHost,
    logDirectory: string,
  ) {
    this.log = new DiagnosticLog(logDirectory, homedir());
    this.tray = new AppTray(
      app.isPackaged
        ? join(process.resourcesPath, "tray")
        : resolve(__dirname, "../packaging/tray"),
      {
        open: () => host.showWindow(),
        newThread: () => {
          host.showWindow();
          host.emit({ type: "menu-new" });
        },
        openAttention: (thread, workspace) => {
          host.showWindow();
          host.emit({ type: "menu-attention", thread, workspace });
        },
        checkUpdates: () => {
          host.showWindow();
          void this.updater.check();
        },
        installUpdate: () => void this.install(),
        quit: () => app.quit(),
        toggleLogin: (enabled) => {
          try {
            this.setLogin(enabled);
          } catch (error) {
            dialog.showErrorBox(
              "Could not change Launch at Login",
              String(error),
            );
          }
          this.tray.render();
        },
        toggleBackground: (enabled) => {
          this.host.save({ background: enabled });
          this.tray.render();
        },
      },
      () => ({
        login: this.login(),
        background: this.background,
        updateReady:
          this.updater?.status.state === "downloaded"
            ? this.updater.status.latest
            : undefined,
      }),
    );
    const automation = process.env.TWR_DESKTOP_TEST_AUTOMATION === "1";
    this.notifier = new Notifier(
      {
        enabled: () => this.notificationsEnabled,
        attended: () => {
          const window = host.window();
          return !!window && window.isVisible() && window.isFocused();
        },
        open: (target) => {
          host.showWindow();
          host.emit({ type: "menu-attention", ...target });
        },
        log: this.log,
      },
      // Automated QA records what would be posted instead of posting it.
      automation ? { post: testPost(), supported: true } : {},
    );
  }

  private startedHidden = false;

  /**
   * Before any window exists: decide whether this launch starts hidden. On
   * Linux the tray host is checked only after the window exists, so a login
   * launch that would hide starts hidden and `probe` shows the window when
   * there turns out to be no tray to bring it back from.
   */
  get startHidden() {
    const atLogin =
      process.argv.includes(AUTOSTART_ARG) ||
      (process.platform === "darwin" &&
        app.getLoginItemSettings().wasOpenedAtLogin === true);
    this.startedHidden =
      atLogin &&
      (process.platform === "linux"
        ? this.host.settings().background === true && this.trayEnabled
        : this.background);
    return this.startedHidden;
  }

  get trayEnabled() {
    return this.host.settings().tray !== false;
  }

  /** Closing the window hides it; the hub connection and terminals stay live. */
  get background() {
    if (this.host.settings().background !== true) return false;
    // macOS keeps a Dock icon; Linux needs a visible status icon to come back.
    return (
      process.platform === "darwin" || (this.trayEnabled && this.trayAvailable)
    );
  }

  private settleUpdateMode!: (
    mode: Promise<{ automatic: boolean; manualReason?: string }>,
  ) => void;

  /** Everything that needs no subprocess; runs before the window is created. */
  start() {
    this.log.info(
      "app",
      `twr ${app.getVersion()} starting (${this.kind}, Electron ${process.versions.electron}, ${process.platform} ${release()})`,
    );
    const mode = new Promise<{ automatic: boolean; manualReason?: string }>(
      (resolve) => (this.settleUpdateMode = resolve),
    );
    this.updater = new Updater({
      kind: this.kind,
      mode,
      current: app.getVersion(),
      log: this.log,
      autoDownload: () => this.autoUpdate,
      // AppImageUpdater renames a versioned image (twr-0.3.0 → twr-0.4.0).
      onAppImageRenamed: (path) => this.refreshAutostart(path),
      onChange: (status) => {
        const window = this.host.window();
        if (window && !window.isDestroyed())
          window.webContents.send("twr:app-update", status);
        this.tray.render();
      },
    });
    this.refreshAutostart(linuxLaunchCommand(process.env, process.execPath));
    // Automated QA never reaches the network on its own schedule.
    const scheduled =
      this.kind !== "development" &&
      process.env.TWR_DESKTOP_TEST_AUTOMATION !== "1";
    if (scheduled && this.autoUpdate)
      setTimeout(
        () => void this.updater.check(),
        FIRST_UPDATE_CHECK_DELAY,
      ).unref();
    this.timer = setInterval(() => {
      if (scheduled && this.autoUpdate) void this.updater.check();
    }, UPDATE_CHECK_INTERVAL);
    this.timer.unref();
    // Other workspaces are read on this beat (shared/App.tsx), also while the window is
    // hidden, when the page's own timers are throttled.
    this.poll = setInterval(
      () => this.host.emit({ type: "inbox-poll" }),
      WORKSPACE_REFRESH_MS,
    );
    this.poll.unref();
    app.on("will-quit", () => {
      if (this.timer) clearInterval(this.timer);
      if (this.poll) clearInterval(this.poll);
    });
    app.on("render-process-gone", (_event, _contents, details) =>
      this.log.error(
        "renderer",
        `Renderer exited: ${details.reason} (code ${details.exitCode})`,
      ),
    );
    app.on("child-process-gone", (_event, details) => {
      if (details.reason !== "clean-exit")
        this.log.warn(
          "process",
          `${details.type} exited: ${details.reason} (code ${details.exitCode})`,
        );
    });
  }

  /**
   * The subprocess checks (the macOS code signature, the Linux tray host), run
   * once the window exists so they never delay it.
   */
  async probe() {
    this.settleUpdateMode(updateMode(this.kind));
    this.trayAvailable = await statusNotifierAvailable();
    if (!this.trayAvailable) {
      this.log.info(
        "tray",
        "No StatusNotifier host yet; checking again while the session starts.",
      );
      // At login, uwsm can start twr before Waybar registers its tray.
      void this.waitForTray();
    }
    this.applyTray();
    // Hidden at login with no tray to come back from: show the window.
    if (this.startedHidden && !this.background) {
      this.log.info("window", "No tray host; showing the window");
      this.host.showWindow();
    }
  }

  get autoUpdate() {
    return this.host.settings().autoUpdate !== false;
  }

  private async waitForTray() {
    for (let attempt = 0; attempt < 24 && !this.trayAvailable; attempt++) {
      await new Promise((resolve) => setTimeout(resolve, 5000));
      if (await statusNotifierAvailable()) {
        this.trayAvailable = true;
        this.log.info("tray", "StatusNotifier host appeared; showing the tray");
        this.applyTray();
      }
    }
  }

  /** Keeps an existing Linux login entry pointing at this executable. */
  private refreshAutostart(command: string) {
    if (process.platform !== "linux" || !app.isPackaged) return;
    try {
      if (refreshLinuxAutostart(this.autostartFile, command))
        this.log.info("login", "Updated the login entry for this executable");
    } catch (error) {
      this.log.warn("login", "Could not update the login entry", error);
    }
  }

  logRejected(channel: string, error: unknown) {
    if (this.rejected.allow())
      this.log.warn("ipc", `Rejected ${channel} message`, error);
  }

  private applyTray() {
    if (this.trayEnabled && this.trayAvailable) {
      try {
        this.tray.show();
      } catch (error) {
        this.trayAvailable = false;
        this.log.warn("tray", "Could not create the status icon", error);
      }
    } else this.tray.hide();
  }

  setAttention(summary: AttentionSummary) {
    this.tray.setAttention(summary);
    this.notifier.update(summary);
    if (testState) testState.attention = summary;
  }

  /** The window came to the front. */
  focused() {
    this.notifier.attend();
  }

  /** The window is gone (not hidden): nothing it could open is left to notify about. */
  closed() {
    this.notifier.clear();
    this.setAttention({ count: 0, items: [], workspaces: [] });
  }

  get notificationsEnabled() {
    return this.host.settings().notifications !== false;
  }

  login(): DesktopAppSettings["login"] {
    if (!app.isPackaged || this.kind === "development")
      return {
        supported: false,
        enabled: false,
        detail: "Available in installed builds.",
      };
    if (process.platform === "darwin") {
      const settings = app.getLoginItemSettings();
      return {
        supported: true,
        enabled: settings.openAtLogin,
        detail:
          settings.status === "requires-approval"
            ? "Allow twr in System Settings › General › Login Items."
            : undefined,
      };
    }
    if (process.platform === "linux")
      return {
        supported: true,
        enabled: linuxAutostartEnabled(this.autostartFile),
        detail:
          "Uses your session's autostart folder (Omarchy, GNOME and KDE start it at login).",
      };
    return { supported: false, enabled: false };
  }

  setLogin(enabled: boolean) {
    if (!this.login().supported)
      throw new Error("Launch at login is not available in this build");
    if (process.platform === "darwin")
      app.setLoginItemSettings({ openAtLogin: enabled });
    else
      setLinuxAutostart(
        this.autostartFile,
        enabled,
        linuxLaunchCommand(process.env, process.execPath),
      );
    this.log.info("login", `Launch at login ${enabled ? "on" : "off"}`);
  }

  snapshot(): DesktopAppSettings {
    return {
      login: this.login(),
      tray: { enabled: this.trayEnabled, available: this.trayAvailable },
      notifications: {
        enabled: this.notificationsEnabled,
        supported: this.notifier.supported,
      },
      background: this.background,
      autoUpdate: this.autoUpdate,
      update: this.updater.status,
      logDirectory: this.log.directory,
    };
  }

  async install() {
    if (this.updater.status.state !== "downloaded")
      throw new Error("No update has been downloaded yet");
    if (this.host.activeTerminals() > 0) {
      // A sheet on a hidden window would be invisible.
      this.host.showWindow();
      const window = this.host.window();
      const options = {
        type: "question" as const,
        message: "Restart twr to finish updating?",
        detail:
          "Interactive programs in open terminals will exit. The hub and its agents keep running.",
        buttons: ["Not now", "Restart and update"],
        defaultId: 1,
        cancelId: 0,
      };
      const { response } = window
        ? await dialog.showMessageBox(window, options)
        : await dialog.showMessageBox(options);
      if (response !== 1) return;
    }
    this.host.prepareQuit();
    try {
      this.updater.install();
    } catch (error) {
      this.host.cancelQuit();
      this.log.error("updater", "Could not start the update", error);
      throw error;
    }
  }

  diagnostics() {
    const gpu = (() => {
      try {
        return app.getGPUFeatureStatus();
      } catch {
        return undefined;
      }
    })();
    const metrics = app.getAppMetrics();
    const facts = {
      version: app.getVersion(),
      install: this.kind,
      automaticUpdates: this.updater.status.automatic,
      update: {
        state: this.updater.status.state,
        latest: this.updater.status.latest,
        error: this.updater.status.error,
      },
      electron: process.versions.electron,
      chrome: process.versions.chrome,
      os: `${process.platform} ${release()} ${process.arch}`,
      locale: app.getLocale(),
      session:
        process.platform === "linux"
          ? {
              type: process.env.XDG_SESSION_TYPE,
              desktop: process.env.XDG_CURRENT_DESKTOP,
              hyprland: !!process.env.HYPRLAND_INSTANCE_SIGNATURE,
              wayland: !!process.env.WAYLAND_DISPLAY,
            }
          : undefined,
      tray: { enabled: this.trayEnabled, available: this.trayAvailable },
      notifications: {
        enabled: this.notificationsEnabled,
        supported: this.notifier.supported,
      },
      background: this.background,
      launchAtLogin: this.login().enabled,
      memoryMB: Math.round(
        metrics.reduce((sum, metric) => sum + metric.memory.workingSetSize, 0) /
          1024,
      ),
      processes: metrics.length,
      crashReports: countCrashReports(app.getPath("crashDumps")),
      gpu,
      uptimeMinutes: Math.round(process.uptime() / 60),
      ...this.host.facts(),
    };
    return diagnosticsReport(facts, this.log.tail(200), homedir());
  }

  register(
    handle: (channel: string, callback: (...args: any[]) => any) => void,
    listen: (channel: string, callback: (...args: any[]) => void) => void,
  ) {
    handle("app-settings", () => this.snapshot());
    handle("app-login", (enabled) => {
      this.setLogin(z.boolean().parse(enabled));
      this.tray.render();
      return this.snapshot();
    });
    handle("app-tray", (enabled) => {
      this.host.save({ tray: z.boolean().parse(enabled) });
      this.applyTray();
      return this.snapshot();
    });
    handle("app-notifications", (enabled) => {
      const value = z.boolean().parse(enabled);
      this.host.save({ notifications: value });
      if (!value) this.notifier.clear();
      this.log.info("notify", `Notifications ${value ? "on" : "off"}`);
      return this.snapshot();
    });
    handle("app-background", (enabled) => {
      this.host.save({ background: z.boolean().parse(enabled) });
      this.tray.render();
      return this.snapshot();
    });
    handle("app-auto-update", (enabled) => {
      this.host.save({ autoUpdate: z.boolean().parse(enabled) });
      return this.snapshot();
    });
    handle("app-update-check", () => this.updater.check());
    handle("app-update-download", () => this.updater.download());
    handle("app-update-install", () => this.install());
    handle("app-diagnostics", () => this.diagnostics());
    handle("app-open-logs", async () => {
      const error = await shell.openPath(this.log.directory);
      if (error) throw new Error(error);
    });
    listen("app-attention", (summary) =>
      this.setAttention(attentionSummarySchema.parse(summary)),
    );
    listen("app-error", (report) => {
      if (!this.errors.allow()) return;
      const value = errorSchema.parse(report);
      this.log.error(
        "renderer",
        `${value.source ? value.source + ": " : ""}${value.message}`,
        value.stack,
      );
    });
  }
}

/**
 * What automated QA reads of the notifications and the tray (scripts/integration.mjs, in
 * the main process): what was posted, closed and clicked, and the latest summary.
 */
interface TestState {
  attention?: AttentionSummary;
  notifications: {
    title: string;
    body: string;
    target: object;
    closed: boolean;
    click(): void;
  }[];
}
let testState: TestState | undefined;
function testPost(): Post {
  const state: TestState = { notifications: [] };
  testState = state;
  (globalThis as { __twrTest?: TestState }).__twrTest = state;
  return (notice, handlers) => {
    const record = {
      title: notice.title,
      body: notice.body,
      target: notice.target,
      closed: false,
      click: () => handlers.click(),
    };
    state.notifications.push(record);
    return { close: () => (record.closed = true) };
  };
}

function readPackageType() {
  try {
    return readFileSync(
      join(process.resourcesPath, "package-type"),
      "utf8",
    ).trim();
  } catch {
    return undefined;
  }
}

async function updateMode(
  kind: AppServices["kind"],
): Promise<{ automatic: boolean; manualReason?: string }> {
  if (kind === "appimage") return { automatic: true };
  if (kind === "mac")
    return (await macDeveloperIdSigned())
      ? { automatic: true }
      : {
          automatic: false,
          manualReason:
            "This build is not signed with a Developer ID, which macOS requires to replace the app automatically.",
        };
  const manualReason =
    kind === "deb"
      ? "Debian packages are updated through apt."
      : kind === "pacman"
        ? "Arch packages are updated through pacman or your AUR helper."
        : kind === "portable"
          ? "This portable copy is replaced by hand."
          : kind === "development"
            ? "Development builds are not updated."
            : undefined;
  return { automatic: false, manualReason };
}

function macDeveloperIdSigned(): Promise<boolean> {
  if (process.platform !== "darwin" || !app.isPackaged)
    return Promise.resolve(false);
  const bundle = resolve(process.execPath, "../../..");
  return new Promise((done) =>
    execFile(
      "/usr/bin/codesign",
      ["--display", "--verbose=2", bundle],
      { timeout: 10000 },
      (_error, stdout, stderr) =>
        done(developerIdSigned(String(stdout) + String(stderr))),
    ),
  );
}
