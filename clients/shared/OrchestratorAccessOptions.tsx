import { useCallback, useEffect, useId, useRef, useState } from "react";
import { errorMessage } from "./errors";
import {
  ACCESS_CHOICES,
  FULL_ACCESS_CONFIRM,
  accessLevel,
  accessOutcome,
  unknownMethod,
  type OrchestratorAccess,
} from "./orchestrator-access";
import type { Json } from "./types";
import { Dialog } from "./ui";

/** Settings › Orchestrator's access level (`orchestrator.access`). Shown only when the hub
 * reports one (`hub.info`/`orchestrator.show` `access`): an older hub has none, and nothing
 * here offers to change it. Full asks first; Limited doesn't. The parent shows it only to
 * the user's own clients and devices with `admin`, the callers the hub accepts. */
export function OrchestratorAccessOptions({
  rpc,
  live,
  refreshKey,
  changed,
}: {
  rpc: (method: string, params?: Json) => Promise<Json>;
  /** `hub.info`'s `orchestrator.access`, kept current by the app's own refresh. */
  live?: unknown;
  /** A reconnect or another change: read the level again. */
  refreshKey?: number;
  /** The level changed here: the app reads `hub.info` again. */
  changed?: () => void;
}) {
  const ids = useId();
  const [access, setAccess] = useState(() => accessLevel(live));
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [outcome, setOutcome] = useState<{ text: string; warning?: string }>();
  const [error, setError] = useState("");
  const [unsupported, setUnsupported] = useState(false);
  // A read that a newer change overtook doesn't put the old level back.
  const generation = useRef(0);
  useEffect(() => {
    const level = accessLevel(live);
    if (level) setAccess(level);
  }, [live]);
  const read = useCallback(() => {
    const request = generation.current;
    return rpc("orchestrator.show")
      .then((show) => {
        const level = accessLevel(show?.access);
        if (level && request === generation.current) setAccess(level);
      })
      .catch(() => {});
  }, [rpc]);
  useEffect(() => {
    void read();
  }, [read, refreshKey]);
  const choose = (next: OrchestratorAccess) => {
    const request = ++generation.current;
    setBusy(true);
    setError("");
    setOutcome(undefined);
    void rpc("orchestrator.access", { access: next })
      .then((result) => {
        if (request !== generation.current) return;
        setAccess(accessLevel(result?.access) ?? next);
        setOutcome(accessOutcome(result));
        changed?.();
        return read();
      })
      .catch((e) => {
        if (request !== generation.current) return;
        if (unknownMethod(e)) setUnsupported(true);
        else setError(errorMessage(e));
      })
      .finally(() => {
        if (request === generation.current) setBusy(false);
      });
  };
  if (!access) return null;
  return (
    <div className="field orchestrator-access">
      <span id={`${ids}-label`}>Orchestrator access</span>
      <div
        className="segmented"
        role="group"
        aria-labelledby={`${ids}-label`}
        aria-busy={busy || undefined}
      >
        {ACCESS_CHOICES.map((choice) => (
          <button
            type="button"
            key={choice.value}
            className={access === choice.value ? "active" : ""}
            aria-pressed={access === choice.value}
            aria-describedby={`${ids}-${choice.value}`}
            disabled={busy || unsupported}
            onClick={() => {
              if (choice.value === access) return;
              if (choice.value === "full") setConfirming(true);
              else choose(choice.value);
            }}
          >
            {choice.label}
          </button>
        ))}
      </div>
      {ACCESS_CHOICES.map((choice) => (
        <small key={choice.value} id={`${ids}-${choice.value}`}>
          <b>{choice.label}:</b> {choice.description}
        </small>
      ))}
      {unsupported && (
        <p className="error-text" role="alert">
          This hub can't change the orchestrator's access. Update twr on it, or
          set <code>[orchestrator] access</code> in its configuration and
          restart the hub.
        </p>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <p className="success-text" role="status">
        {outcome?.text ?? ""}
      </p>
      {outcome?.warning && (
        <p className="error-text" role="alert">
          {outcome.warning}
        </p>
      )}
      {confirming && (
        <Dialog
          title={FULL_ACCESS_CONFIRM.title}
          subtitle={FULL_ACCESS_CONFIRM.detail}
          onClose={() => setConfirming(false)}
        >
          <footer className="dialog-footer">
            <span />
            <div>
              <button onClick={() => setConfirming(false)}>Cancel</button>
              <button
                className="danger"
                onClick={() => {
                  setConfirming(false);
                  choose("full");
                }}
              >
                {FULL_ACCESS_CONFIRM.action}
              </button>
            </div>
          </footer>
        </Dialog>
      )}
    </div>
  );
}
