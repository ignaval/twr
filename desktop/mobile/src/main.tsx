import React, { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { Dialog as NativeDialog } from "@capacitor/dialog";
import { App as NativeApp } from "@capacitor/app";
import App from "../../src/App";
import { Dialog } from "../../src/ui";
import { EditorSessionsProvider, useEditorSessions } from "../../src/editors";
import { RemoteHost, Transport, type Hub } from "./transport";
import { observeLayout } from "./layout";
import {
  notificationListeners,
  testNotification,
  enablePush,
  refreshPush,
  pushEnabled,
  pushPreferenceKey,
} from "./notifications";
import "../../src/style.css";
import "./mobile.css";

function Mobile() {
  const editors = useEditorSessions();
  const [hubs, setHubs] = useState<Hub[]>([]);
  const [active, setActive] = useState<Hub>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [pairing, setPairing] = useState(false);
  const [hubSheet, setHubSheet] = useState(false);
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
  const activeHub = useRef<Hub>(undefined);
  const [readyHubId, setReadyHubId] = useState<string>();
  const [pendingNotification, setPendingNotification] =
    useState<Record<string, string>>();
  async function load() {
    try {
      setHubs((await Transport.listHubs()).hubs);
    } catch (e) {
      setError(String(e));
    }
  }
  useEffect(() => {
    void load();
    return () => {
      void host.current?.stop();
    };
  }, []);
  useEffect(() => {
    const stop = observeLayout();
    return () => {
      void stop.then((fn) => fn());
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
        })().catch((error) => setError(String(error)));
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
    const listener = NativeApp.addListener("appStateChange", ({ isActive }) => {
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
                  if (host.current === remote) setNotice(String(error));
                });
            })
            .catch((error) => {
              if (host.current === remote) setError(String(error));
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
  async function disconnect() {
    await host.current?.stop();
    host.current = null;
    activeHub.current = undefined;
    setActive(undefined);
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
      if (generation === folderGeneration.current) setError(String(error));
    }
  }
  async function connect(hub: Hub) {
    setReadyHubId(undefined);
    setBusy(true);
    setError("");
    try {
      await host.current?.stop();
      const remote = new RemoteHost(
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
      window.twr = remote.api();
      document.documentElement.dataset.mobile = "true";
      document.documentElement.dataset.hub = hub.id;
      activeHub.current = hub;
      setActive(hub);
      return true;
    } catch (e) {
      setError(String(e));
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
      setError(String(e));
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
        onReady={() => {
          setReadyHubId(active.id);
          const remote = host.current;
          if (remote && activeHub.current?.id === active.id)
            void refreshPush(remote, active.id).catch((error) =>
              setNotice(String(error)),
            );
        }}
      />
      <button
        className="mobile-hub-switch"
        onClick={() => setHubSheet(true)}
        aria-label="Remote hub settings"
      >
        {active.name}
      </button>
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
            {notice && <p role="status">{notice}</p>}
            <button
              onClick={() =>
                void testNotification()
                  .then(() => setNotice("Local notification sent."))
                  .catch((e) => setNotice(String(e)))
              }
            >
              Test notification
            </button>
            <button
              onClick={() => {
                if (host.current)
                  void enablePush(host.current, active.id)
                    .then(() => setNotice("Remote push enabled."))
                    .catch((e) => setNotice(String(e)));
              }}
            >
              Enable remote push
            </button>
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
                  await host.current?.request("unpair");
                  await Transport.removeHub({ id: active.id });
                  localStorage.removeItem(pushPreferenceKey(active.id));
                  await disconnect();
                  await load();
                  setHubSheet(false);
                })().catch((error) => setNotice(String(error)))
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
            <button
              disabled={busy}
              onClick={async () => {
                await Transport.removeHub({ id: hub.id });
                editors.forget(hub.id);
                localStorage.removeItem(`twr.mobile.drafts.${hub.id}`);
                localStorage.removeItem(pushPreferenceKey(hub.id));
                await load();
              }}
            >
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
          <label>
            Hub name
            <input
              name="hubName"
              required
              maxLength={80}
              placeholder="My workstation"
            />
          </label>
          <label>
            Hub URL
            <input
              name="url"
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
              required
              autoCapitalize="none"
              autoCorrect="off"
              autoComplete="off"
              spellCheck={false}
            />
          </label>
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
    </main>
  );
}
document.documentElement.dataset.mobile = "true";
createRoot(document.getElementById("root")!).render(
  <EditorSessionsProvider>
    <Mobile />
  </EditorSessionsProvider>,
);
