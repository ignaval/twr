import { useEditorSessions } from "../shared/editors";
import { forgetOfflineHost } from "../shared/offline";
import { useEffect, useRef, useState } from "react";
import QRCode from "qrcode";
import { Dialog } from "../shared/ui";
import { errorMessage } from "../shared/errors";
import { handleRovingKey } from "../shared/a11y";
import {
  encodeInvitation,
  parseInvitation,
  type GatewaySettings,
  type GatewayStatus,
  type Invitation,
} from "../shared/connections";
import type { Hub } from "../shared/remote";
import "./connections.css";

export function Connections({
  active,
  onClose,
  onSelect,
}: {
  active?: Hub;
  onClose(): void;
  onSelect(hub?: Hub): Promise<void>;
}) {
  const editors = useEditorSessions();
  const [hubs, setHubs] = useState<Hub[]>([]);
  const [tab, setTab] = useState<"hubs" | "gateway">("hubs");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [vaultError, setVaultError] = useState(false);
  const [resetHubs, setResetHubs] = useState(false);
  const [notice, setNotice] = useState("");
  const [invitation, setInvitation] = useState<Invitation>();
  const [qr, setQr] = useState("");
  const invitationPanel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (qr) invitationPanel.current?.scrollIntoView({ block: "nearest" });
  }, [qr]);
  const [now, setNow] = useState(Date.now());
  const [settings, setSettings] = useState<GatewaySettings>({
    url: "",
    listen: "0.0.0.0:47321",
    cert: "",
    key: "",
    scopes: "operate",
  });
  const [checked, setChecked] = useState(false);
  const [scopes, setScopes] = useState<GatewaySettings["scopes"]>("operate");
  const [restartRequired, setRestartRequired] = useState(false);
  const [confirmRestart, setConfirmRestart] = useState(false);
  const [confirmStop, setConfirmStop] = useState(false);
  const [gatewayStatus, setGatewayStatus] = useState<GatewayStatus>();
  const probeGateway = async () => {
    const status = await window.twrRemote.gatewayStatus();
    setGatewayStatus(status);
    setRestartRequired((previous) => previous || !!status.restartRequired);
    return status;
  };
  const cancelInvitation = async () => {
    await window.twrRemote.cancelInvitation();
    setInvitation(undefined);
  };
  const close = () => {
    // The hub still enforces expiry if the local gateway cannot be contacted.
    void window.twrRemote.cancelInvitation().catch(() => {});
    onClose();
  };
  const refresh = async () => {
    try {
      const { hubs } = await window.twrRemote.listHubs();
      setHubs(hubs);
      setVaultError(false);
    } catch (error) {
      if (errorMessage(error).includes("SAVED_HUBS_UNREADABLE:"))
        setVaultError(true);
      throw error;
    }
  };
  useEffect(() => {
    void refresh().catch((e) => setError(errorMessage(e)));
    void window.twrRemote
      .settings()
      .then((saved) => {
        if (saved) {
          setSettings(saved);
          setScopes(saved.scopes);
          setChecked(false);
        }
      })
      .catch((e) => setError(errorMessage(e)));
  }, []);
  useEffect(() => {
    void probeGateway().catch((e) => setError(errorMessage(e)));
    return () => {
      void window.twrRemote.cancelInvitation().catch(() => {});
    };
  }, []);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  useEffect(() => {
    let live = true;
    setQr("");
    if (invitation) {
      setNow(Date.now());
      void QRCode.toDataURL(encodeInvitation(invitation), {
        width: 280,
        margin: 2,
        errorCorrectionLevel: "M",
      }).then((value) => {
        if (live) setQr(value);
      });
    }
    return () => {
      live = false;
    };
  }, [invitation]);
  async function run(job: () => Promise<void>) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await job();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }
  function update(key: keyof GatewaySettings, value: string) {
    setSettings((old) => ({ ...old, [key]: value }));
    setChecked(false);
    setRestartRequired(false);
    setConfirmRestart(false);
    void cancelInvitation().catch((e) => setError(errorMessage(e)));
  }
  return (
    <Dialog title="Hubs and remote access" onClose={close}>
      <div className="connections-content">
        <div
          className="button-row"
          role="tablist"
          aria-label="Connection settings"
          onKeyDown={(event) => {
            const next = handleRovingKey(
              event,
              event.currentTarget,
              '[role="tab"]',
              "horizontal",
            );
            if (next?.dataset.tab) setTab(next.dataset.tab as typeof tab);
          }}
        >
          {(
            [
              ["hubs", "Saved hubs"],
              ["gateway", "Share this computer"],
            ] as const
          ).map(([key, label]) => (
            <button
              key={key}
              role="tab"
              id={`connections-tab-${key}`}
              data-tab={key}
              aria-selected={tab === key}
              aria-controls={
                tab === key ? `connections-panel-${key}` : undefined
              }
              tabIndex={tab === key ? 0 : -1}
              onClick={() => setTab(key)}
            >
              {label}
            </button>
          ))}
        </div>
        {error && (
          <p role="alert">{error.replace(/^SAVED_HUBS_UNREADABLE:\s*/, "")}</p>
        )}
        {notice && <p role="status">{notice}</p>}
        {vaultError && (
          <div className="button-row">
            <button
              disabled={busy}
              onClick={() =>
                void run(async () => {
                  await refresh();
                  setError("");
                })
              }
            >
              Retry saved connections
            </button>
            <button disabled={busy} onClick={() => setResetHubs(true)}>
              Reset saved hubs…
            </button>
          </div>
        )}
        <div
          role="tabpanel"
          id={`connections-panel-${tab}`}
          aria-labelledby={`connections-tab-${tab}`}
          className="connections-panel"
        >
          {tab === "hubs" ? (
            <>
              <button
                disabled={busy || !active}
                onClick={() => void run(() => onSelect())}
              >
                This computer{!active ? " · connected" : ""}
              </button>
              {hubs.map((hub) => (
                <article className="saved-hub" key={hub.id}>
                  <div>
                    <strong>{hub.name}</strong>
                    <small>
                      {hub.url} · {hub.scopes.join(", ")}
                    </small>
                  </div>
                  <button
                    disabled={busy || active?.id === hub.id}
                    onClick={() => void run(() => onSelect(hub))}
                  >
                    {active?.id === hub.id ? "Connected" : "Connect"}
                  </button>
                  <button
                    disabled={busy || active?.id === hub.id}
                    onClick={() =>
                      void run(async () => {
                        await forgetOfflineHost(hub.id, window.twrLocal);
                        editors.forget(hub.id);
                        await window.twrRemote.removeHub({ id: hub.id });
                        await refresh();
                      })
                    }
                  >
                    Forget
                  </button>
                  <small>
                    Forget removes this device's local token. Revoke the device
                    on the hub to invalidate any other copies.
                  </small>
                </article>
              ))}
              <form
                className="connection-form"
                onPaste={(event) => {
                  const raw = event.clipboardData.getData("text").trim();
                  if (!raw.startsWith("{")) return;
                  event.preventDefault();
                  try {
                    const invitation = parseInvitation(raw);
                    const form = event.currentTarget;
                    const url = form.elements.namedItem(
                      "url",
                    ) as HTMLInputElement;
                    const code = form.elements.namedItem(
                      "code",
                    ) as HTMLInputElement;
                    const name = form.elements.namedItem(
                      "hubName",
                    ) as HTMLInputElement;
                    url.value = invitation.url;
                    code.value = invitation.code;
                    if (!name.value)
                      name.value = new URL(invitation.url).hostname;
                    setError("");
                    setNotice(
                      "Invitation loaded. Check the hub address and device name, then pair.",
                    );
                  } catch (error) {
                    setError(errorMessage(error));
                  }
                }}
                onSubmit={(event) => {
                  event.preventDefault();
                  const data = new FormData(event.currentTarget);
                  void run(async () => {
                    const hub = await window.twrRemote.pair({
                      url: String(data.get("url")),
                      code: String(data.get("code")).trim(),
                      hubName: String(data.get("hubName")),
                      name: String(data.get("name")),
                    });
                    await refresh();
                    await onSelect(hub);
                  });
                }}
              >
                <h3>Pair another hub</h3>
                <p>
                  Create a one-use invitation on that computer. Paste it here,
                  or enter its address and code.
                </p>
                <label>
                  Hub name
                  <input
                    name="hubName"
                    maxLength={80}
                    required
                    placeholder="Workstation"
                  />
                </label>
                <label>
                  HTTPS hub URL
                  <input
                    name="url"
                    type="url"
                    required
                    placeholder="https://workstation.example:47321"
                    autoCapitalize="none"
                    autoCorrect="off"
                  />
                </label>
                <label>
                  One-use code
                  <input
                    name="code"
                    required
                    autoComplete="off"
                    autoCapitalize="none"
                    spellCheck={false}
                  />
                </label>
                <label>
                  This device
                  <input
                    name="name"
                    required
                    maxLength={80}
                    defaultValue="twr desktop"
                  />
                </label>
                <button className="primary" disabled={busy}>
                  {busy ? "Connecting…" : "Pair and connect"}
                </button>
              </form>
            </>
          ) : (
            <>
              <p>
                Use a hostname reachable from your other device and a TLS
                certificate trusted by it. Your hub and gateway keep running
                after you close the app, until this computer restarts.
              </p>
              <p role="status">
                {gatewayStatus
                  ? gatewayStatus.running
                    ? `Gateway running · ${gatewayStatus.runningVersion} · ${gatewayStatus.listen}`
                    : "Gateway stopped. Starting it is optional."
                  : "Checking gateway status…"}
              </p>
              {gatewayStatus?.renewalState &&
                gatewayStatus.renewalState !== "current" && (
                  <p role="status">
                    Certificate renewal {gatewayStatus.renewalState}.{" "}
                    {gatewayStatus.renewalError} The previous TLS context
                    remains active.
                  </p>
                )}
              {gatewayStatus?.logTail && <pre>{gatewayStatus.logTail}</pre>}
              <button
                disabled={busy}
                onClick={() =>
                  void run(async () => {
                    await probeGateway();
                  })
                }
              >
                Refresh gateway status
              </button>
              {gatewayStatus?.running && (
                <button disabled={busy} onClick={() => setConfirmStop(true)}>
                  Stop gateway…
                </button>
              )}
              <form
                className="connection-form"
                onSubmit={(event) => {
                  event.preventDefault();
                  void run(async () => {
                    await cancelInvitation();
                    const result = await window.twrRemote.setup({
                      ...settings,
                      scopes,
                    });
                    setRestartRequired(!!result.restartRequired);
                    setChecked(!result.restartRequired);
                    await probeGateway();
                    setNotice(
                      result.advisory ||
                        "Local TLS, hostname and gateway identity checks passed. Check the address from your other device before pairing.",
                    );
                  });
                }}
              >
                <label>
                  Public HTTPS address
                  <input
                    required
                    type="url"
                    disabled={busy}
                    value={settings.url}
                    onChange={(e) => update("url", e.target.value)}
                    placeholder="https://workstation.example:47321"
                  />
                </label>
                <label>
                  Listen IP and port
                  <input
                    required
                    disabled={busy}
                    value={settings.listen}
                    onChange={(e) => update("listen", e.target.value)}
                  />
                </label>
                <label>
                  TLS certificate path
                  <input
                    required
                    disabled={busy}
                    value={settings.cert}
                    onChange={(e) => update("cert", e.target.value)}
                    placeholder="/absolute/path/fullchain.pem"
                  />
                </label>
                <label>
                  TLS private key path
                  <input
                    required
                    disabled={busy}
                    value={settings.key}
                    onChange={(e) => update("key", e.target.value)}
                    placeholder="/absolute/path/privkey.pem"
                  />
                </label>
                <label>
                  Device access
                  <select
                    disabled={busy}
                    value={scopes}
                    onChange={(e) => {
                      setScopes(e.target.value as GatewaySettings["scopes"]);
                      void cancelInvitation().catch((e) =>
                        setError(errorMessage(e)),
                      );
                    }}
                  >
                    <option value="read">Read only</option>
                    <option value="operate">Read and operate</option>
                    <option value="terminal">
                      Read, operate and terminals
                    </option>
                    <option value="admin">
                      Read, operate, terminals and settings
                    </option>
                  </select>
                </label>
                <button disabled={busy} className="primary">
                  {busy ? "Checking…" : "Start gateway and check connection"}
                </button>
              </form>
              {restartRequired && (
                <button
                  disabled={busy}
                  onClick={() => {
                    if (!confirmRestart) {
                      setConfirmRestart(true);
                      return;
                    }
                    void run(async () => {
                      await cancelInvitation();
                      const result = await window.twrRemote.restart({
                        ...settings,
                        scopes,
                      });
                      setRestartRequired(!!result.restartRequired);
                      setChecked(!result.restartRequired);
                      setConfirmRestart(false);
                      await probeGateway();
                      setNotice(
                        result.advisory ||
                          "Gateway restarted with the selected configuration. The independent hub is still running.",
                      );
                    });
                  }}
                >
                  {confirmRestart
                    ? "Confirm restart · close gateway terminal sessions"
                    : "Restart gateway with these settings"}
                </button>
              )}
              {confirmRestart && (
                <p role="alert">
                  Restarting disconnects phones and closes their gateway
                  terminal sessions. The independent hub keeps running.
                </p>
              )}
              {checked && (
                <button
                  disabled={busy}
                  onClick={() =>
                    void run(async () =>
                      setInvitation(
                        await window.twrRemote.invite({
                          scopes,
                        }),
                      ),
                    )
                  }
                >
                  Create one-use invitation
                </button>
              )}
              {invitation &&
                (invitation.expiresAt > now ? (
                  <div className="pair-invitation" ref={invitationPanel}>
                    {qr && (
                      <img
                        src={qr}
                        alt="Scan this one-use twr pairing invitation with your phone"
                        width={280}
                        height={280}
                      />
                    )}
                    <p>On your phone, choose Pair a hub → Scan QR code.</p>
                    <code>{invitation.code}</code>
                    <p>{invitation.url}</p>
                    <p>
                      Granted access: {invitation.scopes?.join(", ") || scopes}.
                    </p>
                    <small>
                      One use · expires in{" "}
                      {Math.ceil((invitation.expiresAt - now) / 1000)} seconds.
                      Creating another invitation replaces this one.
                    </small>
                  </div>
                ) : (
                  <p role="status">
                    Invitation expired. Create a new one to pair a device.
                  </p>
                ))}
            </>
          )}
        </div>
        {confirmStop && (
          <Dialog
            title="Stop remote access?"
            onClose={() => setConfirmStop(false)}
          >
            <p>
              This disconnects paired devices and closes gateway terminal
              sessions. The hub and its threads keep running. You can start the
              gateway again with new settings.
            </p>
            <button disabled={busy} onClick={() => setConfirmStop(false)}>
              Cancel
            </button>
            <button
              disabled={busy}
              onClick={() =>
                void run(async () => {
                  await window.twrRemote.stopGateway();
                  setChecked(false);
                  setRestartRequired(false);
                  setConfirmRestart(false);
                  setInvitation(undefined);
                  setConfirmStop(false);
                  await probeGateway();
                })
              }
            >
              Stop gateway
            </button>
          </Dialog>
        )}
        {resetHubs && (
          <Dialog title="Reset saved hubs?" onClose={() => setResetHubs(false)}>
            <p>
              This removes saved hub credentials on this computer so you can
              pair again. It keeps this computer's hub and encrypted offline
              history. It does not revoke devices on the remote hubs.
            </p>
            <button disabled={busy} onClick={() => setResetHubs(false)}>
              Cancel
            </button>
            <button
              disabled={busy}
              onClick={() =>
                void run(async () => {
                  await window.twrRemote.resetSavedHubs();
                  await refresh();
                  setResetHubs(false);
                  await onSelect();
                })
              }
            >
              Reset saved hubs
            </button>
          </Dialog>
        )}
      </div>
    </Dialog>
  );
}
