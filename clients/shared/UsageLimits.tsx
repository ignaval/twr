// Usage limits: how much of each subscription window is left, per runtime and account
// (usage-limits.ts holds the arithmetic). Each window is a row: its name and the share left,
// a bar filled to that share with a tick where even spending would be, and when it resets.
// Shown in three places: `/usage` opens it as a panel above the message box, the gauge in
// the conversation's header as a popover, and the Usage page in full.
import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { ArrowUpRight, Gauge, RefreshCw, X } from "lucide-react";
import { errorMessage } from "./errors";
import type { Json } from "./types";
import {
  accountsOf,
  ageText,
  type AccountView,
  type WindowView,
} from "./usage-limits";
import { Spinner } from "./ui";
import "./usage.css";

/** While one is on screen, data is read again this often, besides on the hub's news. */
const REFRESH_MS = 60_000;
/** Relative times ("resets in 2h", "as of 12m ago") are redrawn this often. */
const TICK_MS = 30_000;

/**
 * `usage.get`, read while `active`: on opening, when `revision` changes (the hub's
 * live view carries every account's usage, and changes it when an agent reports new
 * numbers), and once a minute; never while closed.
 */
export function useUsage(
  rpc: (method: string, params?: Json) => Promise<Json>,
  active: boolean,
  revision?: string,
) {
  const [data, setData] = useState<Json>();
  const [fetchedAt, setFetchedAt] = useState(0);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const reading = useRef(0);
  const load = useCallback(async () => {
    const read = ++reading.current;
    setLoading(true);
    try {
      const answer = await rpc("usage.get");
      if (read !== reading.current) return;
      setData(answer);
      setFetchedAt(Date.now());
      setNow(Date.now());
      setError("");
    } catch (e) {
      if (read === reading.current) setError(errorMessage(e));
    } finally {
      if (read === reading.current) setLoading(false);
    }
  }, [rpc]);
  useEffect(() => {
    if (active) void load();
  }, [active, revision, load]);
  useEffect(() => {
    if (!active) return;
    const refresh = setInterval(() => void load(), REFRESH_MS);
    const tick = setInterval(() => setNow(Date.now()), TICK_MS);
    return () => {
      clearInterval(refresh);
      clearInterval(tick);
      reading.current++;
    };
  }, [active, load]);
  return {
    accounts: data ? accountsOf(data, now, fetchedAt) : undefined,
    error,
    loading,
    reload: load,
  };
}

/** One window: name and share left, the bar with its even-pace tick, the reset. */
function WindowRow({
  account,
  window,
}: {
  account: AccountView;
  window: WindowView;
}) {
  return (
    <div className="usage-row" data-level={window.level} title={window.title}>
      <div className="usage-row-head">
        <span>{window.name}</span>
        <b>{window.leftText}</b>
      </div>
      <div
        className="usage-bar"
        role="meter"
        aria-label={`${account.label} ${window.name}`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(window.left)}
        aria-valuetext={window.valueText}
      >
        <span
          className="usage-fill"
          style={{ width: `${window.left}%` } as CSSProperties}
        />
        {window.pace !== undefined && (
          <span
            className="usage-pace"
            style={{ left: `${window.pace}%` } as CSSProperties}
          />
        )}
      </div>
      {window.reset && (
        <small className="usage-reset" title={`Resets ${window.resetAt}`}>
          {window.reset}
        </small>
      )}
    </div>
  );
}

/**
 * The Usage page's limits: `data`, a `usage.get` answer read at `fetchedAt`, as it is now.
 * The clock goes on between reads and while a read fails, as in the panel and the popover:
 * countdowns, ages and pace advance, and a window that has reset since goes.
 */
export function UsagePage({
  data,
  fetchedAt,
}: {
  data: Json;
  fetchedAt: number;
}) {
  // Redrawn every tick; the page's own reads redraw it too.
  const [, setTick] = useState(0);
  useEffect(() => {
    const tick = setInterval(() => setTick((n) => n + 1), TICK_MS);
    return () => clearInterval(tick);
  }, []);
  return <UsageLimits accounts={accountsOf(data, Date.now(), fetchedAt)} />;
}

/** Every account's windows, or why there are none. */
export function UsageLimits({
  accounts,
  error,
  compact = false,
}: {
  accounts?: AccountView[];
  error?: string;
  compact?: boolean;
}) {
  if (!accounts)
    return error ? (
      <p className="usage-error" role="status">
        {error}
      </p>
    ) : (
      <p className="usage-empty" aria-busy="true">
        Reading usage…
      </p>
    );
  if (!accounts.length)
    return (
      <>
        {error && (
          <p className="usage-error" role="status">
            {error}
          </p>
        )}
        <p className="usage-empty">
          No current limits to show. A window shows here once an agent session
          reports it, and goes when it resets until a session reports it again.
        </p>
      </>
    );
  return (
    <div className={`usage-limits ${compact ? "compact" : ""}`}>
      {error && (
        <p className="usage-error" role="status">
          {error}
        </p>
      )}
      {accounts.map((account) => (
        <section
          className="usage-account"
          key={account.key}
          data-stale={account.stale || undefined}
          aria-label={account.label}
        >
          <header>
            <b>{account.label}</b>
            {account.plan && <span> · {account.plan}</span>}
            {account.stale && (
              <span className="usage-age">
                as of {ageText(account.age)} ago
              </span>
            )}
          </header>
          {account.windows.map((window) => (
            <WindowRow key={window.name} account={account} window={window} />
          ))}
        </section>
      ))}
    </div>
  );
}

/** What the panel and the popover both say: the limits, and the way to the page. */
function UsageBody({
  usage,
  onOpenPage,
}: {
  usage: ReturnType<typeof useUsage>;
  onOpenPage: () => void;
}) {
  return (
    <>
      <UsageLimits accounts={usage.accounts} error={usage.error} compact />
      <button type="button" className="usage-page-link" onClick={onOpenPage}>
        Open in Activity <ArrowUpRight size={13} aria-hidden="true" />
      </button>
    </>
  );
}

/** `/usage`: the limits in a panel above the message box, until you close it. */
export function UsagePanel({
  rpc,
  revision,
  onClose,
  onOpenPage,
}: {
  rpc: (method: string, params?: Json) => Promise<Json>;
  revision?: string;
  onClose: () => void;
  onOpenPage: () => void;
}) {
  const usage = useUsage(rpc, true, revision);
  const heading = useId();
  const panel = useRef<HTMLElement>(null);
  useEffect(() => {
    // Opened by a command typed in the message box: say what opened.
    panel.current?.focus();
  }, []);
  return (
    <section
      className="usage-panel"
      ref={panel}
      tabIndex={-1}
      aria-labelledby={heading}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          onClose();
        }
      }}
    >
      <header>
        <h2 id={heading}>
          <Gauge size={15} aria-hidden="true" /> Usage limits
        </h2>
        <button
          type="button"
          className="icon-button"
          aria-label="Refresh usage limits"
          title="Refresh"
          aria-busy={usage.loading || undefined}
          onClick={() => void usage.reload()}
        >
          {usage.loading ? <Spinner /> : <RefreshCw size={14} />}
        </button>
        <button
          type="button"
          className="icon-button"
          aria-label="Close usage limits"
          title="Close"
          onClick={onClose}
        >
          <X size={15} />
        </button>
      </header>
      <UsageBody usage={usage} onOpenPage={onOpenPage} />
    </section>
  );
}

/** The gauge in the conversation's header: the limits in a popover. */
export function UsageButton({
  rpc,
  revision,
  connected,
  onOpenPage,
}: {
  rpc: (method: string, params?: Json) => Promise<Json>;
  revision?: string;
  connected: boolean;
  onOpenPage: () => void;
}) {
  const [open, setOpen] = useState(false);
  const usage = useUsage(rpc, open && connected, revision);
  const opener = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const ids = useId();
  useEffect(() => {
    if (!open) return;
    panel.current?.focus();
    // A click or the focus (Tab) outside it closes it: left open, Esc no longer reaches it.
    const away = (event: Event) => {
      const target = event.target as Node;
      if (!panel.current?.contains(target) && !opener.current?.contains(target))
        setOpen(false);
    };
    document.addEventListener("mousedown", away);
    document.addEventListener("focusin", away);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("focusin", away);
    };
  }, [open]);
  const close = () => {
    setOpen(false);
    opener.current?.focus();
  };
  return (
    <div
      className="usage-popover-anchor"
      // Esc closes it from the gauge too: Shift-Tab from the popover lands there, and the
      // focus check leaves the gauge out (a click on it toggles the popover).
      onKeyDown={(event) => {
        if (open && event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          close();
        }
      }}
    >
      <button
        type="button"
        ref={opener}
        className={`icon-button ${open ? "on" : ""}`}
        aria-label="Usage limits"
        title="Usage limits"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? `${ids}-popover` : undefined}
        disabled={!connected}
        onClick={() => setOpen((v) => !v)}
      >
        <Gauge size={16} />
      </button>
      {open && (
        <div
          className="usage-popover"
          id={`${ids}-popover`}
          ref={panel}
          role="dialog"
          aria-label="Usage limits"
          tabIndex={-1}
        >
          <h2>Usage limits</h2>
          <UsageBody
            usage={usage}
            onOpenPage={() => {
              setOpen(false);
              onOpenPage();
            }}
          />
        </div>
      )}
    </div>
  );
}
