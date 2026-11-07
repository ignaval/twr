import { autoUpdater } from "electron-updater";
import type { DiagnosticLog } from "./diagnostics";
import {
  compareVersions,
  installerFor,
  manualInstructions,
  updateMetadataName,
  type InstallKind,
  type ReleaseAsset,
  type UpdateStatus,
} from "./update-policy";

const RELEASES = "https://api.github.com/repos/ignaval/twr/releases/latest";

/**
 * One update flow for every install kind. The GitHub release is always read
 * first: electron-updater is only engaged for self-updating installs, and only
 * once the release actually carries their update metadata (desktop assets are
 * attached after the CLI release is published).
 */
export class Updater {
  status: UpdateStatus;
  private checking?: Promise<UpdateStatus>;
  private wired = false;
  constructor(
    private readonly options: {
      kind: InstallKind;
      automatic: boolean;
      manualReason?: string;
      current: string;
      log: DiagnosticLog;
      onChange(status: UpdateStatus): void;
      onAppImageRenamed?(path: string): void;
      autoDownload(): boolean;
      fetch?: typeof fetch;
    },
  ) {
    this.status = {
      kind: options.kind,
      automatic: options.automatic,
      manualReason: options.manualReason,
      state: "idle",
      current: options.current,
    };
  }
  private set(update: Partial<UpdateStatus>) {
    this.status = { ...this.status, ...update };
    this.options.onChange(this.status);
  }
  private wire() {
    if (this.wired) return;
    this.wired = true;
    const log = this.options.log;
    autoUpdater.logger = {
      info: (message?: unknown) => log.info("updater", String(message)),
      warn: (message?: unknown) => log.warn("updater", String(message)),
      error: (message?: unknown) => log.error("updater", String(message)),
      debug: () => {},
    };
    autoUpdater.allowPrerelease = false;
    autoUpdater.allowDowngrade = false;
    autoUpdater.autoInstallOnAppQuit = true;
    autoUpdater.on("download-progress", (progress) =>
      this.set({
        state: "downloading",
        progress: Math.round(progress.percent),
      }),
    );
    autoUpdater.on("appimage-filename-updated", (path: string) =>
      this.options.onAppImageRenamed?.(path),
    );
    autoUpdater.on("update-not-available", () =>
      this.set({ state: "up-to-date", instructions: undefined }),
    );
    autoUpdater.on("update-downloaded", (info) =>
      this.set({ state: "downloaded", latest: info.version, progress: 100 }),
    );
    autoUpdater.on("error", (error) => {
      log.error("updater", "Automatic update failed", error);
      this.set({ state: "error", error: describe(error) });
    });
  }
  check(): Promise<UpdateStatus> {
    if (
      this.status.state === "downloading" ||
      this.status.state === "downloaded"
    )
      return Promise.resolve(this.status);
    this.checking ??= this.run().finally(() => (this.checking = undefined));
    return this.checking;
  }
  private async run(): Promise<UpdateStatus> {
    const { kind, log } = this.options;
    if (kind === "development") {
      this.set({
        state: "idle",
        instructions: manualInstructions(kind, undefined),
        checkedAt: Date.now(),
      });
      return this.status;
    }
    this.set({ state: "checking", error: undefined });
    try {
      const response = await (this.options.fetch || fetch)(RELEASES, {
        headers: { Accept: "application/vnd.github+json" },
        signal: AbortSignal.timeout(15000),
      });
      if (!response.ok)
        throw new Error(`GitHub answered ${response.status}. Try again later.`);
      const release = (await response.json()) as {
        tag_name?: string;
        html_url?: string;
        assets?: { name: string; browser_download_url: string }[];
      };
      if (!release.tag_name) throw new Error("The latest release has no tag");
      const latest = release.tag_name.replace(/^v/, "");
      const assets: ReleaseAsset[] = (release.assets || []).map((asset) => ({
        name: asset.name,
        url: asset.browser_download_url,
      }));
      const newer = compareVersions(latest, this.options.current) > 0;
      const download = installerFor(kind, assets);
      const base = {
        latest,
        releaseUrl: release.html_url,
        download,
        checkedAt: Date.now(),
      };
      log.info(
        "updater",
        `Installed ${this.options.current}; latest ${latest}; ${kind}`,
      );
      if (!newer) {
        this.set({ ...base, state: "up-to-date", instructions: undefined });
        return this.status;
      }
      const metadata = updateMetadataName(kind);
      const selfUpdating =
        this.options.automatic &&
        !!metadata &&
        assets.some((asset) => asset.name === metadata);
      if (!selfUpdating) {
        this.set({
          ...base,
          state: "available",
          instructions:
            this.options.automatic && !download
              ? "This release's desktop installers are still being published. Check again shortly."
              : manualInstructions(kind, download),
        });
        return this.status;
      }
      this.wire();
      autoUpdater.autoDownload = this.options.autoDownload();
      // Stay "checking" until electron-updater has its own update info, so a
      // Download request cannot arrive before it can be served.
      this.set({ ...base, instructions: undefined });
      const result = await autoUpdater.checkForUpdates();
      if (this.status.state === "checking")
        this.set({
          state:
            result?.isUpdateAvailable === false ? "up-to-date" : "available",
        });
      return this.status;
    } catch (error) {
      log.warn("updater", "Update check failed", error);
      this.set({ state: "error", error: describe(error) });
      return this.status;
    }
  }
  async download() {
    if (!this.options.automatic || this.status.state !== "available")
      throw new Error("No automatic update is waiting to download");
    this.wire();
    this.set({ state: "downloading", progress: 0 });
    await autoUpdater.downloadUpdate();
    return this.status;
  }
  install() {
    if (this.status.state !== "downloaded")
      throw new Error("No update has been downloaded yet");
    this.options.log.info("updater", `Restarting into ${this.status.latest}`);
    // isSilent=false, isForceRunAfter=true: reopen twr after installing.
    autoUpdater.quitAndInstall(false, true);
  }
}

function describe(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  if (/ENOTFOUND|ECONNREFUSED|ETIMEDOUT|network|TimeoutError/i.test(message))
    return "Could not reach GitHub. Check your connection and try again.";
  return message.split("\n")[0].slice(0, 300);
}
