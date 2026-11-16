import { forgetOfflineHost } from "../../shared/offline";
import { initializeLayout } from "../../shared/layoutPreferences";
import { AppearanceOptions } from "../../shared/AppearanceOptions";
import { errorMessage } from "../../shared/errors";
import { Keyboard } from "@capacitor/keyboard";
import { initializeAppearance } from "../../shared/appearance";
import { observeNativeAppearance } from "./appearance";
import React, { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { Dialog as NativeDialog } from "@capacitor/dialog";
import { App as NativeApp } from "@capacitor/app";
import App from "../../shared/App";
import { Dialog } from "../../shared/ui";
import { ErrorBoundary } from "../../shared/ErrorBoundary";
import {
  EditorSessionsProvider,
  useEditorSessions,
} from "../../shared/editors";
import { RemoteHost, Transport, sweepExports, type Hub } from "./transport";
import {
  autoConnectTarget,
  forgetLastHub,
  linkState,
  readLastHub,
  rememberLastHub,
  type LinkState,
} from "./connection";
import { mobileVoice } from "./voice";
import { observeLayout } from "./layout";
import { observeTextScale, secureDelete } from "./offline";
import { parseInvitation } from "../../shared/connections";
import {
  notificationListeners,
  clearDeliveredNotifications,
  testNotification,
  enablePush,
  disablePush,
  refreshPush,
  pushEnabled,
  remotePushBuilt,
  pushPreferenceKey,
} from "./notifications";
import "../../shared/style.css";
import "./mobile.css";

initializeLayout();

function Mobile() {
  const editors = useEditorSessions();
  const [hubs, setHubs] = useState<Hub[]>([]);
  const [active, setActive] = useState<Hub>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [pairing, setPairing] = useState(false);
  const [pairName, setPairName] = useState("");
  const [scanned, setScanned] = useState<{
    url: string;
    code: string;
    expiresAt?: number;
  }>();
  const [pairingNow, setPairingNow] = useState(Date.now());
  useEffect(() => {
    if (!pairing) {
      setScanned(undefined);
      setPairName("");
      return;
    }
    if (!scanned?.expiresAt) return;
    setPairingNow(Date.now());
    const timer = setInterval(() => setPairingNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [pairing, scanned?.expiresAt]);
  const [hubSheet, setHubSheet] = useState(false);
  const [link, setLink] = useState<LinkState>("reconnecting");
  const [notice, setNotice] = useState("");
  const [folder, setFolder] = useState<{
    path: string;
    entries: { name: string; path: string }[];
    parent?: string;
    verified?: string;
    truncated?: boolean;
    resolve(value: string | null): void;
  }>();
  const folderGeneration = useRef(0);
  const host = useRef<RemoteHost>(null);
  const voice = useRef<ReturnType<typeof mobileVoice>>(undefined);
  const switchTail = useRef<Promise<unknown>>(Promise.resolve());
  function queueSwitch<T>(job: () => Promise<T>): Promise<T> {
    const pending = switchTail.current.catch(() => {}).then(job);
    switchTail.current = pending;
    return pending;
  }
  const activeHub = useRef<Hub>(undefined);
  const [readyHubId, setReadyHubId] = useState<string>();
  const [pendingNotification, setPendingNotification] =
    useState<Record<string, string>>();
  const autoConnectTried = useRef(false);
  async function load() {
    try {
      const { hubs, notice } = await Transport.listHubs();
      setHubs(hubs);
      if (notice) setError(notice);
      return hubs;
    } catch (e) {
      setError(errorMessage(e));
      return undefined;
    }
  }
  useEffect(() => {
    void load().then((hubs) => {
      // Cold launch: back to the hub used last, once. Unreachable, it opens with its
      // saved history like a tap on Connect; a failure leaves the hub list and its error.
      const target = autoConnectTarget(
        hubs || [],
        readLastHub(),
        autoConnectTried.current,
      );
      autoConnectTried.current = true;
      if (target)
        void queueSwitch(() =>
          // A notification tap may have chosen a hub meanwhile.
          activeHub.current ? Promise.resolve(false) : connectNow(target),
        );
    });
    // Exports a previous run left behind (the app was killed while sharing).
    void sweepExports();
    return () => {
      void host.current?.stop();
    };
  }, []);
  useEffect(() => {
    const textScale = observeTextScale();
    const stop = observeLayout();
    return () => {
      void stop.then((fn) => fn());
      void textScale.then((fn) => fn());
    };
  }, []);
  useEffect(() => {
    const stop = notificationListeners(
      (data) => {
        void (async () => {
          if (
            data.gateway_id &&
            activeHub.current?.gateway_id !== data.gateway_id
          ) {
            const hub = (await Transport.listHubs()).hubs.find(
              (hub) => hub.gateway_id === data.gateway_id,
            );
            if (!hub)
              throw new Error(
                "This notification belongs to a hub that is no longer paired.",
              );
            if (!(await connect(hub))) return;
          }
          setHubSheet(false);
          setFolder((old) => {
            old?.resolve(null);
            return undefined;
          });
          if (host.current) {
            await host.current.resume();
            setPendingNotification(data);
          }
        })().catch((error) => setError(errorMessage(error)));
      },
      () =>
        activeHub.current && pushEnabled(activeHub.current.id)
          ? host.current || undefined
          : undefined,
    );
    return () => {
      void stop.then((fn) => fn());
    };
  }, []);
  useEffect(() => {
    if (active && readyHubId === active.id && pendingNotification) {
      window.dispatchEvent(
        new CustomEvent("twr:open-attention", { detail: pendingNotification }),
      );
      setPendingNotification(undefined);
    }
  }, [active, readyHubId, pendingNotification]);
  useEffect(() => {
    const navigate = (event: Event) => {
      const detail = (
        event as CustomEvent<{ hubId: string; workspace?: string }>
      ).detail;
      void (async () => {
        if (activeHub.current?.id !== detail.hubId) {
          const hub = (await Transport.listHubs()).hubs.find(
            (hub) => hub.id === detail.hubId,
          );
          if (!hub) throw new Error("Saved hub no longer exists");
          if (!(await connect(hub))) return;
        }
        setPendingNotification(
          detail.workspace ? { workspace: detail.workspace } : {},
        );
      })().catch((error) => setError(errorMessage(error)));
    };
    window.addEventListener("twr:select-host", navigate);
    return () => window.removeEventListener("twr:select-host", navigate);
  }, []);
  useEffect(() => {
    void clearDeliveredNotifications();
    const listener = NativeApp.addListener("appStateChange", ({ isActive }) => {
      if (isActive) void clearDeliveredNotifications();
      if (host.current) {
        host.current.setForeground(isActive);
        if (isActive) {
          const remote = host.current;
          const hubId = activeHub.current?.id;
          void remote
            .resume()
            .then(() => {
              if (hubId)
                return refreshPush(remote, hubId).catch((error) => {
                  if (host.current === remote) setNotice(errorMessage(error));
                });
            })
            .catch((error) => {
              if (host.current === remote) setError(errorMessage(error));
            });
        } else
          void host.current
            .request("rpc", {
              method: "notify.looking",
              params: { looking: false },
            })
            .catch(() => {});
      }
    });
    return () => {
      void listener.then((handle) => handle.remove());
    };
  }, []);
  /** An explicit disconnect (Switch hub): the next launch opens the hub list. */
  async function disconnect() {
    rememberLastHub(undefined);
    return queueSwitch(disconnectNow);
  }
  async function disconnectNow() {
    const previous = host.current;
    host.current = null;
    activeHub.current = undefined;
    setActive(undefined);
    setReadyHubId(undefined);
    setPendingNotification(undefined);
    setHubSheet(false);
    folderGeneration.current++;
    setFolder((old) => {
      old?.resolve(null);
      return undefined;
    });
    voice.current?.dispose();
    voice.current = undefined;
    Reflect.deleteProperty(window, "twr");
    await previous?.stop();
  }
  async function forgetLocal(hub: Hub) {
    await forgetOfflineHost(hub.id, { secureDelete });
    await sweepExports();
    await Transport.removeHub({ id: hub.id });
    editors.forget(hub.id);
    localStorage.removeItem(`twr.mobile.drafts.${hub.id}`);
    localStorage.removeItem(pushPreferenceKey(hub.id));
    forgetLastHub(hub.id);
  }
  async function forget(hub: Hub) {
    return queueSwitch(() => forgetNow(hub));
  }
  async function forgetNow(hub: Hub) {
    setBusy(true);
    const remote = new RemoteHost(
      hub,
      () => {},
      async () => null,
    );
    try {
      await remote.start();
      await disablePush(remote, hub.id);
    } catch {
      setError(
        "The hub could not be reached to disable push. Revoke this device on that hub to stop any remaining notifications.",
      );
    } finally {
      await remote.stop().catch(() => {});
    }
    try {
      await forgetLocal(hub);
      await load();
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }
  async function browse(path: string) {
    const generation = ++folderGeneration.current;
    try {
      const listing = await host.current?.request("directories", { path });
      if (listing && generation === folderGeneration.current)
        setFolder((old) =>
          old ? { ...old, ...listing, verified: listing.path } : old,
        );
    } catch (error) {
      if (generation === folderGeneration.current)
        setError(errorMessage(error));
    }
  }
  async function connect(hub: Hub) {
    return queueSwitch(() => connectNow(hub));
  }
  async function connectNow(hub: Hub) {
    setReadyHubId(undefined);
    setBusy(true);
    setError("");
    let remote: RemoteHost | undefined;
    try {
      await disconnectNow();
      remote = new RemoteHost(
        hub,
        () => {
          void disconnect();
        },
        () =>
          new Promise((resolve) => {
            setFolder({ path: "/", entries: [], resolve });
            void browse("/");
          }),
      );
      await remote.start();
      host.current = remote;
      const started = remote;
      // The gateway registers push per device and workspace: refresh after the
      // first connection, every in-app workspace switch and every reconnect.
      started.onWorkspaceConnected(() => {
        if (host.current === started) setLink("online");
        if (host.current === started && activeHub.current?.id === hub.id)
          void refreshPush(started, hub.id).catch((error) => {
            if (host.current === started) setNotice(errorMessage(error));
          });
      });
      const api = remote.api();
      setLink(started.socketConnected ? "online" : "reconnecting");
      api.onEvent((event) => {
        if (event.type !== "connection" || host.current !== started) return;
        setLink(linkState(!!event.connected, started.revoked, event.error));
      });
      // Voice mode where the app has the native recognizer and voice (docs/mobile.md).
      voice.current = mobileVoice({
        request: (method, params, timeout) =>
          started.request(method, params, timeout),
        onEvent: api.onEvent,
      });
      if (voice.current) api.nativeVoice = voice.current;
      window.twr = api;
      document.documentElement.dataset.mobile = "true";
      document.documentElement.dataset.hub = hub.id;
      activeHub.current = hub;
      setActive(hub);
      rememberLastHub(hub.id);
      return true;
    } catch (e) {
      await remote?.stop().catch(() => {});
      setError(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  }
  async function pair(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    setBusy(true);
    setError("");
    try {
      const hub = await Transport.pair({
        url: String(data.get("url")),
        code: String(data.get("code")).trim(),
        name: String(data.get("name")),
        hubName: String(data.get("hubName")),
      });
      await load();
      setPairing(false);
      await connect(hub);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }
  return active ? (
    <>
      {error && (
        <div className="mobile-error" role="alert">
          {error}
          <button onClick={() => setError("")}>Dismiss</button>
        </div>
      )}
      <App
        key={active.id}
        deviceScopes={active.scopes}
        onDismissKeyboard={() => {
          document.activeElement instanceof HTMLElement &&
            document.activeElement.blur();
          void Keyboard.hide();
        }}
        headerAccessory={
          <button
            className="mobile-hub-switch"
            onClick={() => setHubSheet(true)}
            aria-label={`Remote hub settings: ${active.name}${link === "online" ? "" : ` (${link})`}`}
          >
            <span className={`status-dot ${link}`} aria-hidden="true" />
            <span className="mobile-hub-name">{active.name}</span>
          </button>
        }
        onReady={() => setReadyHubId(active.id)}
      />

      {hubSheet && (
        <Dialog
          title={active.name}
          subtitle={active.url}
          onClose={() => setHubSheet(false)}
        >
          <div className="mobile-hub-sheet">
            <p className="form-note">
              Device access: {active.scopes.join(" · ")}
            </p>
            <section className="mobile-appearance">
              <h3 id="hub-appearance-title">Appearance</h3>
              <AppearanceOptions labelledBy="hub-appearance-title" />
            </section>
            {notice && <p role="status">{notice}</p>}
            <button
              onClick={() =>
                void testNotification()
                  .then(() => setNotice("Local notification sent."))
                  .catch((e) => setNotice(errorMessage(e)))
              }
            >
              Test notification
            </button>
            {remotePushBuilt() && (
              <button
                onClick={() => {
                  if (host.current)
                    void enablePush(host.current, active.id)
                      .then(() => setNotice("Remote push enabled."))
                      .catch((e) => setNotice(errorMessage(e)));
                }}
              >
                Enable remote push
              </button>
            )}
            {pushEnabled(active.id) && (
              <button
                disabled={busy}
                onClick={() => {
                  if (!host.current) return;
                  setBusy(true);
                  void disablePush(host.current, active.id)
                    .then(() => setNotice("Remote push disabled for this hub."))
                    .catch((error) => setNotice(errorMessage(error)))
                    .finally(() => setBusy(false));
                }}
              >
                Disable remote push
              </button>
            )}
            <button
              onClick={() => {
                setHubSheet(false);
                void disconnect();
              }}
            >
              Switch hub
            </button>
            <button
              className="danger-quiet"
              onClick={() =>
                void (async () => {
                  if (
                    !(
                      await NativeDialog.confirm({
                        title: "Revoke this device?",
                        message:
                          "This device will lose access to this hub. Pair it again to reconnect.",
                      })
                    ).value
                  )
                    return;
                  await queueSwitch(async () => {
                    if (activeHub.current?.id !== active.id || !host.current)
                      throw new Error(
                        "Select this hub again before revoking its device.",
                      );
                    await host.current.request("unpair");
                    await forgetLocal(active);
                    await disconnectNow();
                    await load();
                  });
                })().catch((error) => setNotice(errorMessage(error)))
              }
            >
              Revoke this device
            </button>
            <button onClick={() => setHubSheet(false)}>Done</button>
          </div>
        </Dialog>
      )}
      {folder && (
        <Dialog
          title="Directory on your hub"
          onClose={() => {
            folderGeneration.current++;
            folder.resolve(null);
            setFolder(undefined);
          }}
        >
          <div className="mobile-folder">
            <label>
              Absolute directory
              <input
                autoFocus
                value={folder.path}
                onChange={(e) => {
                  folderGeneration.current++;
                  setFolder({ ...folder, path: e.target.value });
                }}
              />
            </label>
            <button
              disabled={!folder.path.startsWith("/")}
              onClick={() => void browse(folder.path)}
            >
              Browse
            </button>
            <div className="remote-directories">
              {folder.parent && (
                <button onClick={() => void browse(folder.parent!)}>
                  ↑ Parent directory
                </button>
              )}
              {folder.entries.map((entry) => (
                <button
                  key={entry.path}
                  onClick={() => void browse(entry.path)}
                >
                  {entry.name}
                  <span>→</span>
                </button>
              ))}
              {folder.truncated && (
                <p className="form-note">
                  Showing the first 200 directories. Enter a path above to go
                  further.
                </p>
              )}
            </div>
            <div className="modal-footer">
              <button
                onClick={() => {
                  folderGeneration.current++;
                  folder.resolve(null);
                  setFolder(undefined);
                }}
              >
                Cancel
              </button>
              <button
                className="primary"
                disabled={folder.path !== folder.verified}
                onClick={() => {
                  folder.resolve(folder.path);
                  setFolder(undefined);
                }}
              >
                Use directory
              </button>
            </div>
          </div>
        </Dialog>
      )}
    </>
  ) : (
    <main className="mobile-onboarding">
      <div className="wordmark">
        twr<span>_</span>
      </div>
      <p className="eyebrow">YOUR AGENTS, WITH YOU</p>
      <h1>Your hubs</h1>
      <p>
        Follow your work, answer requests, and talk to your threads wherever you
        are.
      </p>
      {error && (
        <p role="alert" className="error-banner">
          {error}
        </p>
      )}
      {hubs.map((hub) => (
        <article className="resource-card" key={hub.id}>
          <button
            className="hub-connect"
            disabled={busy}
            onClick={() => void connect(hub)}
          >
            <b>{hub.name}</b>
            <small>{hub.url}</small>
            <span>Connect →</span>
          </button>
          <details>
            <summary>Device access</summary>
            <p>{hub.scopes.join(" · ")}</p>
            <button disabled={busy} onClick={() => void forget(hub)}>
              Forget this hub
            </button>
            <small>
              Revoke the device on the hub to invalidate its credentials.
            </small>
          </details>
        </article>
      ))}
      {!pairing ? (
        <button
          className="primary"
          disabled={busy}
          onClick={() => setPairing(true)}
        >
          Pair a hub
        </button>
      ) : (
        <form className="pair-form" onSubmit={pair}>
          <h2>Connect to your hub</h2>
          <p>
            Start pairing from your computer, then enter its address and one-use
            code.
          </p>
          <button
            type="button"
            disabled={busy}
            onClick={() => {
              setBusy(true);
              setError("");
              void Transport.scanInvitation()
                .then(({ invitation }) => {
                  const value = parseInvitation(invitation);
                  setScanned(value);
                  setPairName((name) => name || new URL(value.url).hostname);
                })
                .catch((e) => {
                  if (e?.code !== "SCAN_CANCELLED") setError(errorMessage(e));
                })
                .finally(() => setBusy(false));
            }}
          >
            Scan QR code
          </button>
          <label>
            Hub name
            <input
              name="hubName"
              required
              maxLength={80}
              value={pairName}
              onChange={(event) => setPairName(event.target.value)}
              placeholder="My workstation"
            />
          </label>
          <label>
            Hub URL
            <input
              name="url"
              value={scanned?.url ?? ""}
              onChange={(e) =>
                setScanned((old) => ({
                  ...old,
                  url: e.target.value,
                  code: old?.code || "",
                }))
              }
              required
              type="url"
              autoCapitalize="none"
              autoCorrect="off"
              placeholder="https://workstation.example:47321"
            />
          </label>
          <label>
            Pairing code
            <input
              name="code"
              value={scanned?.code ?? ""}
              onChange={(e) =>
                setScanned((old) => ({
                  url: old?.url || "",
                  code: e.target.value,
                }))
              }
              required
              autoCapitalize="none"
              autoCorrect="off"
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          {scanned?.expiresAt && (
            <p role="status">
              {scanned.expiresAt > pairingNow
                ? `Expiry estimate: ${Math.ceil((scanned.expiresAt - pairingNow) / 1000)} seconds. Device clocks can differ; the hub checks expiry.`
                : "This invitation may have expired. The hub will check it when pairing; device clocks can differ."}
            </p>
          )}
          <label>
            This device
            <input
              name="name"
              required
              maxLength={80}
              defaultValue={
                navigator.userAgent.includes("Android")
                  ? "Android"
                  : "iPhone or iPad"
              }
            />
          </label>
          <div className="button-row">
            <button
              type="button"
              disabled={busy}
              onClick={() => setPairing(false)}
            >
              Cancel
            </button>
            <button className="primary" disabled={busy}>
              {busy ? "Connecting…" : "Pair and connect"}
            </button>
          </div>
        </form>
      )}
      <section className="mobile-appearance">
        <h3 id="pairing-appearance-title">Appearance</h3>
        <AppearanceOptions labelledBy="pairing-appearance-title" />
      </section>
    </main>
  );
}
document.documentElement.dataset.mobile = "true";
initializeAppearance();
const stopAppearance = observeNativeAppearance();
import.meta.hot?.dispose(stopAppearance);
createRoot(document.getElementById("root")!).render(
  <ErrorBoundary>
    <EditorSessionsProvider>
      <Mobile />
    </EditorSessionsProvider>
  </ErrorBoundary>,
);
