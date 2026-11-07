import { initializeLayout } from "../shared/layoutPreferences";
import { initializeAppearance } from "../shared/appearance";
import { observeDesktopAppearance } from "./appearance";
import { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import App from "../shared/App";
import "../shared/style.css";
import { EditorSessionsProvider } from "../shared/editors";
import { RemoteHost, type Hub } from "../shared/remote";
import type { DesktopAPI } from "../shared/types";
import type {} from "../shared/connections";
import { Connections } from "./Connections";
import { Dialog } from "../shared/ui";
import { errorMessage } from "../shared/errors";
import { withLocalUploads } from "../shared/local-uploads";
import { withInboxCache } from "../shared/inbox-cache";
const local = withInboxCache(
  withLocalUploads(window.twrLocal, window.twrUploadRequest),
  () => window.twrRemote.listHubs(),
);
let hostEpoch = 0;
function fenced(api: DesktopAPI, epoch: number): DesktopAPI {
  return new Proxy(api, {
    get(target, key) {
      const value = Reflect.get(target, key);
      if (typeof value !== "function") return value;
      return (...args: unknown[]) => {
        if (epoch !== hostEpoch) {
          if (
            [
              "terminalWrite",
              "terminalResize",
              "terminalFocus",
              "terminalAck",
            ].includes(String(key))
          )
            return;
          if (key === "onEvent") return () => {};
          return Promise.reject(new Error("This hub connection was closed"));
        }
        return value.apply(target, args);
      };
    },
  });
}
window.twr = fenced(local, 0);
document.documentElement.dataset.hub = "local";
initializeAppearance();
initializeLayout();
const stopAppearance = observeDesktopAppearance();
import.meta.hot?.dispose(stopAppearance);
function Desktop() {
  const [active, setActive] = useState<Hub>();
  const [revision, setRevision] = useState(0);
  const [mounted, setMounted] = useState(true);
  const [manager, setManager] = useState(false);
  const [error, setError] = useState("");
  const [folder, setFolder] = useState<{
    path: string;
    entries: { name: string; path: string }[];
    parent?: string;
    verified?: string;
    resolve(value: string | null): void;
  }>();
  const remote = useRef<RemoteHost | undefined>(undefined);
  const tail = useRef<Promise<unknown>>(Promise.resolve());
  const starting = useRef<RemoteHost | undefined>(undefined);
  const selection = useRef(0);

  const pendingNavigation = useRef<
    { workspace?: string; attention?: boolean } | undefined
  >(undefined);
  function select(hub?: Hub) {
    const token = ++selection.current;
    void starting.current?.stop();
    const job = tail.current
      .catch(() => {})
      .then(async () => {
        if (token !== selection.current)
          throw new Error("Hub switch cancelled");
        setError("");
        const previous = remote.current;
        let candidate: RemoteHost | undefined;
        let epoch = hostEpoch;
        // A remote transport has one native socket. Fence its old view before replacing it.
        if (previous) {
          epoch = ++hostEpoch;
          setMounted(false);
          remote.current = undefined;
          await previous.stop();
        }
        try {
          let api: DesktopAPI = local;
          if (hub) {
            const boot = await local.bootstrap();
            const host = new RemoteHost(
              hub,
              () => {
                void local.quit();
              },
              () =>
                new Promise((resolve) => {
                  setFolder({ path: "/", entries: [], resolve });
                  void browse("/", host);
                }),
              window.twrRemote,
              {
                platform: boot.platform,
                copyText: local.copyText,
                openExternal: local.openExternal,
                reveal: async (path) =>
                  setError(`Path on ${hub.name}: ${path}`),
                exportText: local.exportText,
                setAppearance: local.setAppearance,
                includeLocalInboxCache: true,
                secureRead: local.secureRead,
                secureWrite: local.secureWrite,
                secureDelete: local.secureDelete,
              },
            );
            candidate = host;
            starting.current = host;
            host.setLooking(document.hasFocus());
            await host.start();
            if (token !== selection.current)
              throw new Error("Hub switch cancelled");
            // Keep local terminals intact until the destination handshake/bootstrap succeeds.
            if (!previous) await window.twrRemote.deactivateLocal();
            const remoteApi = host.api();
            api = {
              ...remoteApi,
              onEvent: (callback) => {
                const offRemote = remoteApi.onEvent(callback);
                const offLocal = local.onEvent((event) => {
                  if (
                    ["menu-new", "menu-palette", "theme"].includes(event.type)
                  )
                    callback(event);
                });
                return () => {
                  offRemote();
                  offLocal();
                };
              },
            };
          }
          if (token !== selection.current)
            throw new Error("Hub switch cancelled");
          if (!previous) {
            epoch = ++hostEpoch;
            setMounted(false);
          }
          setFolder((old) => {
            old?.resolve(null);
            return undefined;
          });
          remote.current = candidate;
          candidate?.setLooking(document.hasFocus());
          starting.current = undefined;
          window.twr = fenced(api, epoch);
          document.documentElement.dataset.hub = hub?.id || "local";
          setActive(hub);
          setRevision((value) => value + 1);
          setManager(false);
          setMounted(true);
        } catch (error) {
          await candidate?.stop().catch(() => {});
          if (starting.current === candidate) starting.current = undefined;
          if (previous) {
            remote.current = undefined;
            window.twr = fenced(local, epoch);
            document.documentElement.dataset.hub = "local";
            setActive(undefined);
            setRevision((value) => value + 1);
            setMounted(true);
          }
          if (token === selection.current) {
            setManager(true);
            setError(errorMessage(error));
          }
          throw error;
        }
      });
    tail.current = job;
    return job;
  }
  async function browse(path: string, host = remote.current) {
    try {
      const listing = await host?.request("directories", { path });
      if (listing && host === remote.current)
        setFolder((old) =>
          old ? { ...old, ...listing, verified: listing.path } : old,
        );
    } catch (e) {
      setError(errorMessage(e));
    }
  }
  useEffect(() => {
    const navigate = (event: Event) => {
      const detail = (event as CustomEvent).detail;
      void (async () => {
        const hub =
          detail.hubId === "local"
            ? undefined
            : (await window.twrRemote.listHubs()).hubs.find(
                (hub) => hub.id === detail.hubId,
              );
        if (detail.hubId !== "local" && !hub)
          throw new Error("Saved hub no longer exists");
        pendingNavigation.current = {
          workspace: detail.workspace,
          attention: detail.attention,
        };
        await select(hub);
      })().catch((e) => setError(errorMessage(e)));
    };
    const focus = () => remote.current?.setLooking(true);
    const blur = () => remote.current?.setLooking(false);
    window.addEventListener("focus", focus);
    window.addEventListener("blur", blur);
    window.addEventListener("twr:select-host", navigate);
    return () => {
      window.removeEventListener("focus", focus);
      window.removeEventListener("blur", blur);
      window.removeEventListener("twr:select-host", navigate);
      void remote.current?.stop();
    };
  }, []);
  return (
    <>
      {mounted && (
        <App
          key={revision}
          deviceScopes={active?.scopes}
          headerAccessory={
            <button
              className="toolbar-button"
              onClick={() => setManager(true)}
              aria-label="Hubs and remote access"
            >
              {active?.name || "This computer"}
            </button>
          }
          onReady={() => {
            const navigation = pendingNavigation.current;
            pendingNavigation.current = undefined;
            if (navigation?.workspace || navigation?.attention)
              window.dispatchEvent(
                new CustomEvent("twr:open-attention", { detail: navigation }),
              );
          }}
        />
      )}
      {manager && (
        <Connections
          active={active}
          onClose={() => {
            selection.current++;
            void starting.current?.stop();
            setManager(false);
          }}
          onSelect={select}
        />
      )}
      {error && (
        <div className="connection-error" role="alert">
          {error}
          <button onClick={() => setError("")}>Dismiss</button>
        </div>
      )}
      {folder && (
        <Dialog
          title="Choose a folder on the hub"
          onClose={() => {
            folder.resolve(null);
            setFolder(undefined);
          }}
        >
          <div className="remote-directory">
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void browse(folder.path);
              }}
            >
              <input
                aria-label="Remote folder path"
                value={folder.path}
                onChange={(e) =>
                  setFolder({
                    ...folder,
                    path: e.target.value,
                    verified: undefined,
                  })
                }
              />
              <button>Open path</button>
            </form>
            {folder.parent && (
              <button onClick={() => void browse(folder.parent!)}>
                Parent folder
              </button>
            )}
            {folder.entries.map((entry) => (
              <button key={entry.path} onClick={() => void browse(entry.path)}>
                {entry.name}
              </button>
            ))}
            <button
              className="primary"
              disabled={folder.verified !== folder.path}
              onClick={() => {
                folder.resolve(folder.path);
                setFolder(undefined);
              }}
            >
              Choose folder
            </button>
          </div>
        </Dialog>
      )}
    </>
  );
}
createRoot(document.getElementById("root")!).render(
  <EditorSessionsProvider>
    <Desktop />
  </EditorSessionsProvider>,
);
