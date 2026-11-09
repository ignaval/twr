import { app, Menu, nativeImage, Tray } from "electron";
import { execFile } from "node:child_process";
import { join } from "node:path";
import type { AttentionSummary } from "../shared/types";
import { attentionKey, trayEntries } from "./tray-state";

export interface TrayActions {
  open(): void;
  newThread(): void;
  /** Opens Needs you, at a thread's item when given, in its workspace when given. */
  openAttention(thread?: string, workspace?: string): void;
  checkUpdates(): void;
  installUpdate(): void;
  quit(): void;
  toggleLogin(enabled: boolean): void;
  toggleBackground(enabled: boolean): void;
}

export interface TrayMenuState {
  login: { supported: boolean; enabled: boolean };
  background: boolean;
  updateReady?: string;
}

/**
 * Linux status icons need a StatusNotifierItem host (Waybar's tray module on
 * Omarchy, KDE, GNOME with the AppIndicator extension). Without one, Electron
 * would create an icon nobody can see, so the app must not hide into it.
 */
export function statusNotifierAvailable(): Promise<boolean> {
  if (process.platform !== "linux") return Promise.resolve(true);
  return new Promise((resolve) => {
    execFile(
      "dbus-send",
      [
        "--session",
        "--print-reply",
        "--dest=org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.NameHasOwner",
        "string:org.kde.StatusNotifierWatcher",
      ],
      { timeout: 3000 },
      (error, stdout) => resolve(!error && /boolean true/.test(stdout)),
    );
  });
}

export function trayLabel(count: number) {
  return count === 0
    ? "Nothing needs you"
    : count === 1
      ? "1 item needs you"
      : `${count} items need you`;
}

export class AppTray {
  private tray?: Tray;
  private attention: AttentionSummary = {
    count: 0,
    items: [],
    workspaces: [],
  };
  private attentionShown = attentionKey(this.attention);
  private badge = 0;
  private images = new Map<string, Electron.NativeImage>();
  /** Whether the Linux icon currently shows its attention variant. */
  private imageAttention?: boolean;
  constructor(
    private readonly iconDirectory: string,
    private readonly actions: TrayActions,
    private readonly state: () => TrayMenuState,
  ) {}
  get active() {
    return !!this.tray && !this.tray.isDestroyed();
  }
  show() {
    if (this.active) return;
    this.tray = new Tray(this.image(false));
    this.imageAttention = false;
    this.tray.setToolTip("twr");
    if (process.platform === "darwin")
      this.tray.setIgnoreDoubleClickEvents(true);
    // Linux status notifiers activate on click; macOS opens the menu itself.
    this.tray.on("click", () => {
      if (process.platform !== "darwin") this.actions.open();
    });
    this.render();
  }
  hide() {
    this.tray?.destroy();
    this.tray = undefined;
  }
  setAttention(summary: AttentionSummary) {
    this.attention = summary;
    // Dock (macOS) and Unity launcher (Linux) badges.
    if (summary.count !== this.badge) {
      this.badge = summary.count;
      app.setBadgeCount(summary.count);
    }
    const key = attentionKey(summary);
    if (key === this.attentionShown) return;
    this.attentionShown = key;
    this.render();
  }
  render() {
    if (!this.active) return;
    const tray = this.tray!;
    const { count } = this.attention;
    tray.setToolTip(count ? `twr — ${trayLabel(count)}` : "twr");
    if (process.platform === "darwin")
      tray.setTitle(count ? String(count) : "");
    // Each new image is a DBus round trip on Linux: send it only on a change.
    else if (this.imageAttention !== count > 0) {
      this.imageAttention = count > 0;
      tray.setImage(this.image(count > 0));
    }
    tray.setContextMenu(this.menu(this.attention));
  }
  private image(attention: boolean) {
    const name =
      process.platform === "darwin"
        ? "trayTemplate.png"
        : attention
          ? "tray-attention.png"
          : "tray.png";
    let image = this.images.get(name);
    if (!image) {
      image = nativeImage.createFromPath(join(this.iconDirectory, name));
      if (process.platform === "darwin") image.setTemplateImage(true);
      this.images.set(name, image);
    }
    return image;
  }
  private menu(summary: AttentionSummary) {
    const state = this.state();
    const { entries, more } = trayEntries(summary);
    const template: Electron.MenuItemConstructorOptions[] = [
      { label: trayLabel(summary.count), enabled: false },
      ...entries.map((entry) =>
        entry.kind === "workspace"
          ? { label: entry.label, enabled: false }
          : {
              label: entry.label,
              click: () =>
                this.actions.openAttention(entry.thread, entry.workspace),
            },
      ),
    ];
    if (more)
      template.push({
        label: "Show all in Needs you…",
        click: () => this.actions.openAttention(),
      });
    template.push(
      { type: "separator" },
      { label: "Open twr", click: () => this.actions.open() },
      { label: "New Thread", click: () => this.actions.newThread() },
      { type: "separator" },
      {
        label: "Launch at Login",
        type: "checkbox",
        enabled: state.login.supported,
        checked: state.login.enabled,
        click: (item) => this.actions.toggleLogin(item.checked),
      },
      {
        label:
          process.platform === "darwin"
            ? "Keep Running When Window Is Closed"
            : "Keep Running in Tray When Window Is Closed",
        type: "checkbox",
        checked: state.background,
        click: (item) => this.actions.toggleBackground(item.checked),
      },
      state.updateReady
        ? {
            label: `Restart to Update to ${state.updateReady}`,
            click: () => this.actions.installUpdate(),
          }
        : {
            label: "Check for Updates…",
            click: () => this.actions.checkUpdates(),
          },
      { type: "separator" },
      { label: "Quit twr", click: () => this.actions.quit() },
    );
    return Menu.buildFromTemplate(template);
  }
}
