import { useEffect, useId, useState } from "react";
import type { DesktopAppSettings, UpdateStatus } from "./types";
import { errorMessage } from "./errors";

/** Desktop-only application settings: login, tray, updates and diagnostics. */
export function AppOptions() {
  const app = window.twrApp;
  const [settings, setSettings] = useState<DesktopAppSettings>();
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!app) return;
    void app
      .settings()
      .then(setSettings)
      .catch((e) => setError(errorMessage(e)));
    return app.onUpdate((update) =>
      setSettings((old) => (old ? { ...old, update } : old)),
    );
  }, [app]);
  if (!app || !settings) return null;
  const change = (task: () => Promise<DesktopAppSettings>) => {
    setError("");
    void task()
      .then(setSettings)
      .catch((e) => setError(errorMessage(e)));
  };
  const linux = navigator.userAgent.includes("Linux");
  return (
    <>
      <section className="settings-section app-options">
        <h2>Application</h2>
        <p>
          Closing the app never stops the hub or its agents. These settings
          apply to this computer.
        </p>
        <Toggle
          label="Launch at login"
          detail={settings.login.detail}
          checked={settings.login.enabled}
          disabled={!settings.login.supported}
          onChange={(value) => change(() => app.setLaunchAtLogin(value))}
        />
        <Toggle
          label={linux ? "Show tray icon" : "Show in menu bar"}
          detail={
            settings.tray.available
              ? "Shows how many items need you, with quick links to them."
              : "No system tray was found. On Omarchy, Waybar's tray module provides one; on GNOME, the AppIndicator extension."
          }
          checked={settings.tray.enabled && settings.tray.available}
          disabled={!settings.tray.available}
          onChange={(value) => change(() => app.setTray(value))}
        />
        <Toggle
          label="Notify me of new requests"
          detail={
            settings.notifications.supported
              ? "A permission or question in any workspace, while twr isn't in front. Notifications name the thread and workspace, never what is asked."
              : "This system doesn't support notifications."
          }
          checked={
            settings.notifications.enabled && settings.notifications.supported
          }
          disabled={!settings.notifications.supported}
          onChange={(value) => change(() => app.setNotifications(value))}
        />
        <Toggle
          label="Keep running when the window is closed"
          detail={
            linux && !(settings.tray.enabled && settings.tray.available)
              ? "Needs the tray icon, so you can bring the window back."
              : "Terminals and notifications stay live. Quit from the tray or with the Quit shortcut."
          }
          checked={settings.background}
          disabled={
            linux && !(settings.tray.enabled && settings.tray.available)
          }
          onChange={(value) => change(() => app.setBackground(value))}
        />
      </section>
      <section className="settings-section app-options">
        <h2>Updates</h2>
        <UpdateSummary status={settings.update} />
        <Toggle
          label={
            settings.update.automatic
              ? "Download updates automatically"
              : "Check for updates automatically"
          }
          detail={
            settings.update.automatic
              ? "Updates install when you restart twr."
              : settings.update.manualReason
          }
          checked={settings.autoUpdate}
          disabled={settings.update.kind === "development"}
          onChange={(value) => change(() => app.setAutoUpdate(value))}
        />
        <div className="manager-actions">
          <button
            disabled={
              settings.update.state === "checking" ||
              settings.update.state === "downloading" ||
              settings.update.kind === "development"
            }
            onClick={() => {
              setError("");
              void app
                .checkForUpdates()
                .then((update) => setSettings({ ...settings, update }))
                .catch((e) => setError(errorMessage(e)));
            }}
          >
            Check now
          </button>
          {settings.update.state === "available" &&
            settings.update.automatic && (
              <button
                className="primary"
                onClick={() =>
                  void app
                    .downloadUpdate()
                    .catch((e) => setError(errorMessage(e)))
                }
              >
                Download {settings.update.latest}
              </button>
            )}
          {settings.update.state === "available" &&
            !settings.update.automatic &&
            settings.update.download && (
              <button
                className="primary"
                onClick={() =>
                  void window.twrLocal
                    .openExternal(settings.update.download!.url)
                    .catch((e) => setError(errorMessage(e)))
                }
              >
                Download {settings.update.download.name}
              </button>
            )}
          {settings.update.state === "downloaded" && (
            <button
              className="primary"
              onClick={() =>
                void app.installUpdate().catch((e) => setError(errorMessage(e)))
              }
            >
              Restart to update
            </button>
          )}
        </div>
      </section>
      <section className="settings-section app-options">
        <h2>Diagnostics</h2>
        <p>
          twr keeps a local log and crash reports on this computer. Nothing is
          sent anywhere. Copied diagnostics have tokens, keys and your home
          folder removed. Check them before sharing.
        </p>
        <p className="app-options-path">
          Log folder: <code>{settings.logDirectory}</code>
        </p>
        <div className="manager-actions">
          <button
            disabled={busy}
            onClick={() => {
              setBusy(true);
              setError("");
              void app
                .diagnostics()
                .then((text) => window.twrLocal.copyText(text))
                .then(() => {
                  setCopied(true);
                  setTimeout(() => setCopied(false), 2500);
                })
                .catch((e) => setError(errorMessage(e)))
                .finally(() => setBusy(false));
            }}
          >
            {copied ? "Copied" : "Copy diagnostics"}
          </button>
          <button
            onClick={() =>
              void app.openLogs().catch((e) => setError(errorMessage(e)))
            }
          >
            Open log folder
          </button>
        </div>
        <span className="sr-only" role="status">
          {copied ? "Diagnostics copied to the clipboard" : ""}
        </span>
      </section>
      {error && (
        <p className="app-options-error" role="alert">
          {error}
        </p>
      )}
    </>
  );
}

function Toggle({
  label,
  detail,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  detail?: string;
  checked: boolean;
  disabled?: boolean;
  onChange(value: boolean): void;
}) {
  const id = useId();
  return (
    <div className="app-option">
      <label htmlFor={id}>
        <span>{label}</span>
        {detail && <small id={`${id}-detail`}>{detail}</small>}
      </label>
      <input
        id={id}
        type="checkbox"
        role="switch"
        checked={checked}
        disabled={disabled}
        aria-describedby={detail ? `${id}-detail` : undefined}
        onChange={(e) => onChange(e.target.checked)}
      />
    </div>
  );
}

function UpdateSummary({ status }: { status: UpdateStatus }) {
  const when = status.checkedAt
    ? new Date(status.checkedAt).toLocaleString()
    : undefined;
  let text = `Version ${status.current}.`;
  if (status.state === "checking") text += " Checking for updates…";
  else if (status.state === "up-to-date")
    text += ` You have the latest version${when ? ` (checked ${when})` : ""}.`;
  else if (status.state === "available")
    text += ` Version ${status.latest} is available.${status.instructions ? " " + status.instructions : ""}`;
  else if (status.state === "downloading")
    text += ` Downloading ${status.latest} (${status.progress ?? 0}%).`;
  else if (status.state === "downloaded")
    text += ` Version ${status.latest} is ready. Restart to finish updating.`;
  else if (status.state === "error")
    text += ` The last check failed: ${status.error}`;
  else if (status.instructions) text += ` ${status.instructions}`;
  return <p>{text}</p>;
}
