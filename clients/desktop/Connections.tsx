import { useEditorSessions } from "../shared/editors";
import { forgetOfflineHost } from "../shared/offline";
import { useEffect, useId, useRef, useState } from "react";
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
import {
  fieldsFromInvitation,
  pairRequest,
  withoutInvitation,
  type PairFields,
} from "./pairing";
import "./connections.css";

const DEFAULT_LISTEN = "0.0.0.0:47321";
const PEM = [{ name: "PEM files", extensions: ["pem", "crt", "cer", "key"] }];

/** A TLS file: chosen in the file dialog, its path shown; Edit path types it instead (and
 * is all there is without a file dialog). */
function PemFileField({
  label,
  kind,
  value,
  disabled,
  placeholder,
  onChange,
}: {
  label: string;
  kind: string;
  value: string;
  disabled: boolean;
  placeholder: string;
  onChange(value: string): void;
}) {
  const pick = window.twrLocal?.pickFile;
  const [typing, setTyping] = useState(!pick);
  const id = useId();
  if (typing)
    return (
      <label>
        {label}
        <input
          aria-required="true"
          disabled={disabled}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          spellCheck={false}
          autoCapitalize="none"
          autoCorrect="off"
        />
      </label>
    );
  return (
    <div className="pem-field" role="group" aria-labelledby={id}>
      <span id={id}>{label}</span>
      <code className="pem-path">{value || "None chosen"}</code>
      <div className="pem-actions">
        <button
          type="button"
          disabled={disabled}
          onClick={() =>
            void pick!({ title: `Choose the ${kind}`, filters: PEM }).then(
              (path) => {
                if (path) onChange(path);
              },
            )
          }
        >
          {value ? `Choose another ${kind}…` : `Choose the ${kind}…`}
        </button>
        <button
          type="button"
          disabled={disabled}
          aria-label={`Edit path to the ${kind}`}
          onClick={() => setTyping(true)}
        >
          Edit path
        </button>
      </div>
    </div>
  );
}

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
  // Why Pair and connect failed: said next to the button, which is where the eye is.
  const [pairError, setPairError] = useState("");
  const listenId = useId();
  const errorRef = useRef<HTMLParagraphElement>(null);
  const pairErrorRef = useRef<HTMLParagraphElement>(null);
  // An error said where it is out of view (the dialog scrolled) comes into view.
  useEffect(() => {
    if (error) errorRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [error]);
  useEffect(() => {
    if (pairError) pairErrorRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [pairError]);
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
  const [pairFields, setPairFields] = useState<PairFields>({
    hubName: "",
    url: "",
    code: "",
    name: "twr desktop",
  });
  const [invitationText, setInvitationText] = useState("");
  // What the last invitation loaded filled in, emptied again when its text goes.
  const loaded = useRef<Partial<PairFields>>(undefined);
  const [manual, setManual] = useState(false);
  const [settings, setSettings] = useState<GatewaySettings>({
    url: "",
    listen: DEFAULT_LISTEN,
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
  /** Fills the Pair form from pasted invitation text: "loaded", "invalid" for one that is
   * wrong (said in the error), "none" for text that isn't one at all. */
  function loadInvitation(text: string): "loaded" | "invalid" | "none" {
    try {
      const filled = fieldsFromInvitation(text, pairFields);
      if (!filled) return "none";
      setPairFields(filled);
      loaded.current = {
        url: filled.url,
        code: filled.code,
        hubName: filled.hubName,
      };
      setError("");
      setPairError("");
      setNotice(
        `Invitation from ${filled.hubName} loaded. Check the names under Enter details manually if you like, then pair.`,
      );
      return "loaded";
    } catch (error) {
      setError(errorMessage(error));
      return "invalid";
    }
  }
  /** Paste invitation was cleared, or no longer holds a valid invitation: what the last
   * one filled in goes with it. */
  function forgetInvitation(text: string) {
    const filled = loaded.current;
    loaded.current = undefined;
    setPairFields((old) => withoutInvitation(old, filled));
    if (filled) setNotice("");
    if (!text.trim()) {
      setError("");
      setPairError("");
    }
  }
  const pairField = (
    key: keyof PairFields,
    label: string,
    extra: React.InputHTMLAttributes<HTMLInputElement> = {},
  ) => (
    <label>
      {label}
      <input
        name={key}
        aria-required="true"
        value={pairFields[key]}
        onChange={(e) =>
          setPairFields((old) => ({ ...old, [key]: e.target.value }))
        }
        {...extra}
      />
    </label>
  );
  const gatewayLine = gatewayStatus
    ? gatewayStatus.running
      ? `Remote access is on · gateway ${gatewayStatus.runningVersion} · ${gatewayStatus.listen}`
      : "Remote access is off."
    : "Checking remote access…";
  return (
    <Dialog
      title="Computers"
      subtitle="Connect to another computer's hub, or let your other devices connect to this one."
      onClose={close}
    >
      <div className="connections-content">
        <div
          className="button-row"
          role="tablist"
          aria-label="Computers"
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
              ["hubs", "Connect to another computer"],
              ["gateway", "Let other devices connect"],
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
          <p role="alert" ref={errorRef}>
            {error.replace(/^SAVED_HUBS_UNREADABLE:\s*/, "")}
          </p>
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
                noValidate
                onPaste={(event) => {
                  // An invitation pasted into any of the fields loads it.
                  if (event.target instanceof HTMLTextAreaElement) return;
                  if (
                    loadInvitation(event.clipboardData.getData("text")) !==
                    "none"
                  )
                    event.preventDefault();
                }}
                onSubmit={(event) => {
                  event.preventDefault();
                  // The invitation as its text is now, never what an earlier one filled.
                  const request = pairRequest(invitationText, pairFields);
                  setPairError("");
                  if ("error" in request) {
                    setNotice("");
                    setError("");
                    setPairError(request.error);
                    if (request.manual) setManual(true);
                    return;
                  }
                  const fields = request.fields;
                  void run(async () => {
                    let hub: Hub;
                    try {
                      hub = await window.twrRemote.pair({
                        url: fields.url,
                        code: fields.code.trim(),
                        hubName: fields.hubName,
                        name: fields.name,
                      });
                    } catch (e) {
                      setPairError(errorMessage(e));
                      return;
                    }
                    await refresh();
                    await onSelect(hub);
                  });
                }}
              >
                <h3>Pair with another computer</h3>
                <p>
                  On that computer, open Let other devices connect and create an
                  invitation. Copy it and paste it here.
                </p>
                <label>
                  Paste invitation
                  <textarea
                    rows={3}
                    value={invitationText}
                    spellCheck={false}
                    autoCapitalize="none"
                    autoCorrect="off"
                    placeholder='{"type":"twr-pair", …}'
                    onChange={(e) => {
                      const text = e.target.value;
                      setInvitationText(text);
                      if (loadInvitation(text) !== "loaded")
                        forgetInvitation(text);
                    }}
                  />
                </label>
                <details
                  className="connection-manual"
                  open={manual}
                  onToggle={(e) => setManual(e.currentTarget.open)}
                >
                  <summary>Enter details manually</summary>
                  <div className="connection-manual-fields">
                    {pairField("hubName", "Hub name", {
                      maxLength: 80,
                      placeholder: "Workstation",
                    })}
                    {pairField("url", "HTTPS hub URL", {
                      type: "url",
                      placeholder: "https://workstation.example:47321",
                      autoCapitalize: "none",
                      autoCorrect: "off",
                    })}
                    {pairField("code", "One-use code", {
                      autoComplete: "off",
                      autoCapitalize: "none",
                      spellCheck: false,
                    })}
                    {pairField("name", "This device", { maxLength: 80 })}
                  </div>
                </details>
                {pairError && (
                  <p role="alert" ref={pairErrorRef} className="error-text">
                    {pairError}
                  </p>
                )}
                <button className="primary" disabled={busy}>
                  {busy ? "Connecting…" : "Pair and connect"}
                </button>
              </form>
            </>
          ) : (
            <>
              <p>
                Let your phone or another computer use this computer's hub over
                HTTPS. The hub keeps running after you close the app, until this
                computer restarts.
              </p>
              <div className="gateway-status">
                <p role="status">{gatewayLine}</p>
                <div className="button-row">
                  <button
                    disabled={busy}
                    onClick={() =>
                      void run(async () => {
                        await probeGateway();
                      })
                    }
                  >
                    Check again
                  </button>
                  {gatewayStatus?.running && (
                    <button
                      disabled={busy}
                      onClick={() => setConfirmStop(true)}
                    >
                      Stop gateway…
                    </button>
                  )}
                </div>
              </div>
              {gatewayStatus?.renewalState &&
                gatewayStatus.renewalState !== "current" && (
                  <p role="status">
                    Certificate renewal {gatewayStatus.renewalState}.{" "}
                    {gatewayStatus.renewalError} The previous TLS context
                    remains active.
                  </p>
                )}
              {gatewayStatus?.logTail && (
                <details className="gateway-log">
                  <summary>Details</summary>
                  <pre>{gatewayStatus.logTail}</pre>
                </details>
              )}
              <form
                className="connection-form"
                noValidate
                onSubmit={(event) => {
                  event.preventDefault();
                  const missing = !settings.url.trim()
                    ? "Enter the public HTTPS address."
                    : !settings.cert.trim()
                      ? "Choose the TLS certificate."
                      : !settings.key.trim()
                        ? "Choose the TLS private key."
                        : !settings.listen.trim()
                          ? "Enter the IP and port to listen on, under Advanced."
                          : "";
                  if (missing) {
                    setNotice("");
                    setError(missing);
                    return;
                  }
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
                <ol className="setup-steps">
                  <li>
                    <h3>Address and certificate</h3>
                    <p>
                      Use an address your other device can reach, and a TLS
                      certificate it trusts.
                    </p>
                    <label>
                      Public HTTPS address
                      <input
                        aria-required="true"
                        type="url"
                        disabled={busy}
                        value={settings.url}
                        onChange={(e) => update("url", e.target.value)}
                        placeholder="https://workstation.example:47321"
                      />
                    </label>
                    <PemFileField
                      label="TLS certificate path"
                      kind="certificate"
                      value={settings.cert}
                      disabled={busy}
                      placeholder="/absolute/path/fullchain.pem"
                      onChange={(value) => update("cert", value)}
                    />
                    <PemFileField
                      label="TLS private key path"
                      kind="private key"
                      value={settings.key}
                      disabled={busy}
                      placeholder="/absolute/path/privkey.pem"
                      onChange={(value) => update("key", value)}
                    />
                    <details className="connection-advanced">
                      <summary>Advanced</summary>
                      <label>
                        Listen IP and port
                        <input
                          aria-required="true"
                          aria-describedby={`${listenId}-hint`}
                          disabled={busy}
                          value={settings.listen}
                          onChange={(e) => update("listen", e.target.value)}
                          placeholder={DEFAULT_LISTEN}
                        />
                      </label>
                      <small id={`${listenId}-hint`}>
                        Default: {DEFAULT_LISTEN}, every network.
                      </small>
                    </details>
                  </li>
                  <li>
                    <h3>What devices may do</h3>
                    <label>
                      Device access
                      <select
                        disabled={busy}
                        value={scopes}
                        onChange={(e) => {
                          setScopes(
                            e.target.value as GatewaySettings["scopes"],
                          );
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
                  </li>
                  <li>
                    <h3>Start and invite</h3>
                    <p>
                      Starting checks the certificate, the address and the
                      gateway. Then create a one-use invitation to scan or paste
                      on the other device.
                    </p>
                    <button disabled={busy} className="primary">
                      {busy
                        ? "Checking…"
                        : "Start gateway and check connection"}
                    </button>
                  </li>
                </ol>
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
                  className="primary"
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
                    <p>
                      On your phone, choose Pair a hub → Scan QR code. On
                      another computer, copy the invitation and paste it under
                      Connect to another computer.
                    </p>
                    <code>{invitation.code}</code>
                    <p>{invitation.url}</p>
                    <button
                      type="button"
                      onClick={() =>
                        void window.twrLocal
                          .copyText(encodeInvitation(invitation))
                          .then(() =>
                            setNotice(
                              "Invitation copied. Paste it on the other computer within five minutes.",
                            ),
                          )
                          .catch((e) => setError(errorMessage(e)))
                      }
                    >
                      Copy invitation
                    </button>
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
