import {
  AppImageUpdater,
  autoUpdater,
  type AppUpdater,
  type UpdateDownloadedEvent,
} from "electron-updater";
import type { InstallOptions } from "electron-updater/out/BaseUpdater";
import { basename } from "node:path";
import { unlinkSync } from "node:fs";
import type { DiagnosticLog } from "./diagnostics";
import {
  checkSignedDownload,
  MAX_SUMS_BYTES,
  SIGNED_SUMS,
  SIGNED_SUMS_SIGNATURE,
  SignatureError,
  verifySignedSums,
  type SignedSums,
} from "./release-signature";
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
 * Reads and verifies the release's signed desktop checksums (release-signature.ts).
 * Undefined when the release has none: a release published before they existed, or one
 * whose desktop assets are still being attached.
 */
export async function fetchSignedSums(
  get: typeof fetch,
  tag: string,
  assets: ReleaseAsset[],
): Promise<SignedSums | undefined> {
  const list = assets.find((asset) => asset.name === SIGNED_SUMS);
  const signature = assets.find(
    (asset) => asset.name === SIGNED_SUMS_SIGNATURE,
  );
  if (!list || !signature) return undefined;
  const read = async (asset: ReleaseAsset) => {
    const response = await get(asset.url, {
      signal: AbortSignal.timeout(15000),
    });
    if (!response.ok)
      throw new Error(
        `GitHub answered ${response.status} for ${asset.name}. Try again later.`,
      );
    const body = Buffer.from(await response.arrayBuffer());
    if (body.length > MAX_SUMS_BYTES)
      throw new SignatureError(`${asset.name} is too large`);
    return body;
  };
  const [text, sig] = await Promise.all([read(list), read(signature)]);
  return verifySignedSums(text, sig.toString("utf8"), tag);
}

/**
 * electron-updater's AppImage updater, which checks a download only against the SHA-512 in
 * latest-linux.yml, a file of the same release as the image. This one also requires the
 * image's SHA-256 in the release's signed checksums, when the download completes (before it
 * is offered) and again just before it replaces the running image.
 */
class SignedAppImageUpdater extends AppImageUpdater {
  /** The verified checksums of the release being updated to. */
  signed?: SignedSums;
  private verified?: { file: string; version: string };
  private check(file: string, version: string) {
    checkSignedDownload(this.signed, version, basename(file), file);
  }
  protected dispatchUpdateDownloaded(event: UpdateDownloadedEvent) {
    try {
      this.check(event.downloadedFile, event.version);
    } catch (error) {
      // Thrown here, before electron-updater arms its install-on-quit: nothing installs it.
      this.verified = undefined;
      try {
        unlinkSync(event.downloadedFile);
      } catch {}
      throw new SignatureError(
        `Update refused: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
    this.verified = { file: event.downloadedFile, version: event.version };
    super.dispatchUpdateDownloaded(event);
  }
  protected doInstall(options: InstallOptions): boolean {
    const file = this.installerPath;
    if (!file || !this.verified || this.verified.file !== file)
      throw new SignatureError(
        "Update refused: the downloaded AppImage was not verified against the release's signed checksums.",
      );
    try {
      this.check(file, this.verified.version);
    } catch (error) {
      throw new SignatureError(
        `Update refused: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
    return super.doInstall(options);
  }
}

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
  private automatic = false;
  private readonly ready: Promise<void>;
  constructor(
    private readonly options: {
      kind: InstallKind;
      /**
       * Whether this install replaces itself. On macOS that needs a code
       * signature check, which runs after the window shows; checks and
       * downloads wait for it.
       */
      mode: Promise<{ automatic: boolean; manualReason?: string }>;
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
      automatic: false,
      state: "idle",
      current: options.current,
    };
    this.ready = options.mode.then(
      ({ automatic, manualReason }) => {
        this.automatic = automatic;
        this.set({ automatic, manualReason });
      },
      (error) => {
        options.log.warn("updater", "Could not tell how to update", error);
        this.set({ manualReason: "This build could not be checked." });
      },
    );
  }
  private set(update: Partial<UpdateStatus>) {
    this.status = { ...this.status, ...update };
    this.options.onChange(this.status);
  }
  private appImage?: SignedAppImageUpdater;
  /** electron-updater's updater for this platform; the signed one for an AppImage. */
  private get engine(): AppUpdater {
    if (this.options.kind !== "appimage") return autoUpdater;
    return (this.appImage ??= new SignedAppImageUpdater());
  }
  private wire() {
    if (this.wired) return;
    this.wired = true;
    const log = this.options.log;
    this.engine.logger = {
      info: (message?: unknown) => log.info("updater", String(message)),
      warn: (message?: unknown) => log.warn("updater", String(message)),
      error: (message?: unknown) => log.error("updater", String(message)),
      debug: () => {},
    };
    this.engine.allowPrerelease = false;
    this.engine.allowDowngrade = false;
    this.engine.autoInstallOnAppQuit = true;
    this.engine.on("download-progress", (progress) =>
      this.set({
        state: "downloading",
        progress: Math.round(progress.percent),
      }),
    );
    this.engine.on("appimage-filename-updated", (path: string) =>
      this.options.onAppImageRenamed?.(path),
    );
    this.engine.on("update-not-available", () =>
      this.set({ state: "up-to-date", instructions: undefined }),
    );
    this.engine.on("update-downloaded", (info) =>
      this.set({ state: "downloaded", latest: info.version, progress: 100 }),
    );
    this.engine.on("error", (error) => {
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
    this.checking ??= this.ready
      .then(() => this.run())
      .finally(() => (this.checking = undefined));
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
        this.automatic &&
        !!metadata &&
        assets.some((asset) => asset.name === metadata);
      if (!selfUpdating) {
        this.set({
          ...base,
          state: "available",
          instructions:
            this.automatic && !download
              ? "This release's desktop installers are still being published. Check again shortly."
              : manualInstructions(kind, download),
        });
        return this.status;
      }
      if (kind === "appimage") {
        // An AppImage installs only what the release's signed checksums name. A release
        // without them (published before they existed, or still being attached) is offered
        // as a manual download, never installed unsigned.
        const signed = await fetchSignedSums(
          this.options.fetch || fetch,
          release.tag_name,
          assets,
        );
        if (!signed) {
          this.set({
            ...base,
            state: "available",
            instructions: `This release has no signed checksums (${SIGNED_SUMS_SIGNATURE}), so twr will not replace itself with it. If it was published just now, check again shortly; otherwise download the AppImage from the release page.`,
          });
          return this.status;
        }
        if (!download || !signed.sums.has(download.name))
          throw new SignatureError(
            `Update refused: ${download?.name ?? "the AppImage"} is not in the release's signed checksums.`,
          );
        (this.engine as SignedAppImageUpdater).signed = signed;
      }
      this.wire();
      this.engine.autoDownload = this.options.autoDownload();
      // Stay "checking" until electron-updater has its own update info, so a
      // Download request cannot arrive before it can be served.
      this.set({ ...base, instructions: undefined });
      const result = await this.engine.checkForUpdates();
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
    if (!this.automatic || this.status.state !== "available")
      throw new Error("No automatic update is waiting to download");
    this.wire();
    this.set({ state: "downloading", progress: 0 });
    await this.engine.downloadUpdate();
    return this.status;
  }
  install() {
    if (this.status.state !== "downloaded")
      throw new Error("No update has been downloaded yet");
    this.options.log.info("updater", `Restarting into ${this.status.latest}`);
    // isSilent=false, isForceRunAfter=true: reopen twr after installing.
    this.engine.quitAndInstall(false, true);
  }
}

function describe(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  if (/ENOTFOUND|ECONNREFUSED|ETIMEDOUT|network|TimeoutError/i.test(message))
    return "Could not reach GitHub. Check your connection and try again.";
  return message.split("\n")[0].slice(0, 300);
}
