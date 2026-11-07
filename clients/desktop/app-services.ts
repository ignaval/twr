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
} from "./autostart";
import {
  DiagnosticLog,
  RateLimiter,
  countCrashReports,
  diagnosticsReport,
} from "./diagnostics";
import { AppTray, statusNotifierAvailable } from "./tray";
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

const attentionSchema = z
  .object({
    count: z.number().int().min(0).max(100000),
    items: z
      .array(
        z
          .object({
            thread: z.string().max(200),
            title: z.string().max(500),
            summary: z.string().max(2000),
          })
          .strict(),
      )
      .max(5),
  })
  .strict();
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
  private trayAvailable = process.platform !== "linux";
  private errors = new RateLimiter(30, 60_000);
  private timer?: ReturnType<typeof setInterval>;
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
        openAttention: (thread) => {
          host.showWindow();
          host.emit({ type: "menu-attention", thread });
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
  }

  /** Before any window exists: decide whether this launch starts hidden. */
  get startHidden() {
    const atLogin =
      process.argv.includes(AUTOSTART_ARG) ||
      (process.platform === "darwin" &&
        app.getLoginItemSettings().wasOpenedAtLogin === true);
    return atLogin && this.background;
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

  async start() {
    this.log.info(
      "app",
      `twr ${app.getVersion()} starting (${this.kind}, Electron ${process.versions.electron}, ${process.platform} ${release()})`,
    );
    let automatic = this.kind === "appimage";
    let manualReason: string | undefined;
    if (this.kind === "mac") {
      automatic = await macDeveloperIdSigned();
      if (!automatic)
        manualReason =
          "This build is not signed with a Developer ID, which macOS requires to replace the app automatically.";
    } else if (this.kind === "deb")
      manualReason = "Debian packages are updated through apt.";
    else if (this.kind === "pacman")
      manualReason =
        "Arch packages are updated through pacman or your AUR helper.";
    else if (this.kind === "portable")
      manualReason = "This portable copy is replaced by hand.";
    else if (this.kind === "development")
      manualReason = "Development builds are not updated.";
    this.updater = new Updater({
      kind: this.kind,
      automatic,
      manualReason,
      current: app.getVersion(),
      log: this.log,
      autoDownload: () => this.autoUpdate,
      onChange: (status) => {
        const window = this.host.window();
        if (window && !window.isDestroyed())
          window.webContents.send("twr:app-update", status);
        this.tray.render();
      },
    });
    this.trayAvailable = await statusNotifierAvailable();
    if (!this.trayAvailable)
      this.log.info(
        "tray",
        "No StatusNotifier host on this session; the tray icon is unavailable.",
      );
    this.applyTray();
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
    app.on("will-quit", () => this.timer && clearInterval(this.timer));
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

  get autoUpdate() {
    return this.host.settings().autoUpdate !== false;
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
      background: this.background,
      autoUpdate: this.autoUpdate,
      update: this.updater.status,
      logDirectory: this.log.directory,
    };
  }

  async install() {
    if (this.host.activeTerminals() > 0) {
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
    this.updater.install();
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
      this.setAttention(attentionSchema.parse(summary)),
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
