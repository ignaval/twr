import { useCallback, useEffect, useId, useState } from "react";
import {
  FolderOpen,
  Pause,
  Play,
  RefreshCw,
  SkipForward,
  Square,
  Volume2,
} from "lucide-react";
import { errorMessage } from "./errors";
import { useHostAPI } from "./host";
import { speechRateChoices } from "./settingsModel";
import { Toggle } from "./Toggle";
import type { Json } from "./types";
import { DataView, Spinner } from "./ui";

type Rpc = (method: string, params?: Json) => Promise<Json>;

/** A hub method's result, read on mount and every `every` ms (none: once), with a way to
 * read it again. */
function useHubRead(rpc: Rpc, method: string, every?: number) {
  const [data, setData] = useState<Json>();
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const load = useCallback(async () => {
    setLoading(true);
    try {
      setData(await rpc(method));
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [rpc, method]);
  useEffect(() => {
    void load();
    if (!every) return;
    const timer = setInterval(() => void load(), every);
    return () => clearInterval(timer);
  }, [load, every]);
  return { data, error, loading, load };
}

function LoadError({ error, retry }: { error: string; retry: () => void }) {
  return (
    <div className="error-banner" role="alert">
      {error}
      <button onClick={retry}>Try again</button>
    </div>
  );
}

/** Settings › Voice › Speakers: the speakers connected to the hub (`speech.status`), each
 * with its own playback controls, whether it reads new messages, and its reading rate. */
export function SpeakersPanel({
  rpc,
  action,
  cliAction,
  fail,
  refreshKey,
}: {
  rpc: Rpc;
  action: (id: string, initial?: Json) => void;
  cliAction: (path: string, initial?: Json) => void;
  fail: (error: unknown) => void;
  refreshKey?: number;
}) {
  const { data, error, loading, load } = useHubRead(rpc, "speech.status", 5000);
  useEffect(() => {
    if (refreshKey) void load();
  }, [refreshKey, load]);
  const speakers: Json[] = data?.speakers || [];
  return (
    <section
      className="settings-section speakers"
      aria-busy={loading || undefined}
    >
      <h2>Speakers</h2>
      <p>Listen through a speaker connected to your hub.</p>
      {error && <LoadError error={error} retry={() => void load()} />}
      <div className="manager-actions">
        <button className="primary" onClick={() => cliAction("speak")}>
          <Volume2 size={15} /> Start a speaker
        </button>
        <button onClick={() => action("read")}>Read messages</button>
      </div>
      {data && !speakers.length && (
        <p className="muted">
          No speakers are connected. Start one to listen on this machine.
        </p>
      )}
      <div className="cards">
        {speakers.map((speaker) => (
          <Speaker
            key={speaker.device || speaker.name}
            speaker={speaker}
            rpc={rpc}
            fail={fail}
            changed={() => void load()}
          />
        ))}
      </div>
      {data && (
        <details className="advanced-details">
          <summary>Speaker diagnostics</summary>
          <DataView value={data} />
        </details>
      )}
    </section>
  );
}

function Speaker({
  speaker,
  rpc,
  fail,
  changed,
}: {
  speaker: Json;
  rpc: Rpc;
  fail: (error: unknown) => void;
  changed: () => void;
}) {
  const ids = useId();
  const [busy, setBusy] = useState(false);
  const control = (op: string, value?: unknown) => {
    setBusy(true);
    void rpc("speech.control", {
      op,
      speaker: speaker.name,
      ...(value === undefined ? {} : { value }),
    })
      .then(changed)
      .catch(fail)
      .finally(() => setBusy(false));
  };
  const state = speaker.error
    ? "Error"
    : speaker.paused
      ? "Paused"
      : speaker.playing
        ? "Reading"
        : "Ready";
  return (
    <article
      className="resource-card speaker-card"
      aria-labelledby={`${ids}-name`}
    >
      <div className="card-heading">
        <Volume2 size={18} />
        <h3 id={`${ids}-name`}>{speaker.name}</h3>
        <span className={`pill ${state === "Ready" ? "success" : ""}`}>
          {state}
        </span>
      </div>
      <p>
        {speaker.engine}
        {speaker.voice ? ` · ${speaker.voice}` : ""}
        {speaker.queued ? ` · ${speaker.queued} queued` : ""}
      </p>
      {speaker.error && <p className="error-text">{speaker.error}</p>}
      <div
        className="card-actions"
        role="group"
        aria-label={`${speaker.name} playback`}
      >
        {speaker.paused ? (
          <button disabled={busy} onClick={() => control("resume")}>
            <Play size={14} aria-hidden="true" /> Resume
          </button>
        ) : (
          <button disabled={busy} onClick={() => control("pause")}>
            <Pause size={14} aria-hidden="true" /> Pause
          </button>
        )}
        <button
          disabled={busy || !speaker.playing}
          onClick={() => control("skip")}
        >
          <SkipForward size={14} aria-hidden="true" /> Skip
        </button>
        <button disabled={busy} onClick={() => control("stop")}>
          <Square size={14} aria-hidden="true" /> Stop
        </button>
      </div>
      <Toggle
        label="Read new messages"
        detail="Reads the orchestrator's new messages as they arrive."
        checked={!!speaker.live}
        disabled={busy}
        onChange={(on) => control("live", on)}
      />
      {typeof speaker.rate === "number" && (
        <div className="setting-field">
          <label htmlFor={`${ids}-rate`}>Reading speed</label>
          <select
            id={`${ids}-rate`}
            value={String(speaker.rate)}
            disabled={busy}
            onChange={(e) => control("rate", Number(e.target.value))}
          >
            {speechRateChoices(speaker.rate).map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </select>
        </div>
      )}
    </article>
  );
}

/** Settings › Advanced › Hub & storage: the engine and its database (`hub.info`), and the
 * hub's maintenance. */
export function HubStoragePanel({
  rpc,
  action,
  cliAction,
  fail,
  refreshKey,
}: {
  rpc: Rpc;
  action: (id: string, initial?: Json) => void;
  cliAction: (path: string, initial?: Json) => void;
  fail: (error: unknown) => void;
  refreshKey?: number;
}) {
  const hostApi = useHostAPI();
  const { data, error, loading, load } = useHubRead(rpc, "hub.info");
  useEffect(() => {
    if (refreshKey) void load();
  }, [refreshKey, load]);
  return (
    <section className="settings-section hub-storage">
      <div className="settings-section-heading">
        <h2>Hub & storage</h2>
        <button
          className="icon-button"
          onClick={() => void load()}
          aria-label="Refresh hub information"
          title="Refresh"
          aria-busy={loading || undefined}
        >
          {loading ? <Spinner /> : <RefreshCw size={16} />}
        </button>
      </div>
      <p>The hub keeps running when you close this window.</p>
      {error && <LoadError error={error} retry={() => void load()} />}
      {data && (
        <div className="cards">
          <article className="resource-card">
            <h3>Engine</h3>
            <p>
              twr {data.version} · {data.workspace}
            </p>
            <p>
              {data.threads} threads · {data.open_attention} items need you
            </p>
            {data.started_at && (
              <small>
                Running since {new Date(data.started_at).toLocaleString()}
              </small>
            )}
          </article>
          <article className="resource-card">
            <h3>Storage</h3>
            <p className="path">{data.db}</p>
            <button onClick={() => void hostApi.reveal(data.db).catch(fail)}>
              Show database folder <FolderOpen size={13} />
            </button>
          </article>
        </div>
      )}
      {data?.warnings?.map((w: string) => (
        <p className="error-banner" key={w}>
          {w}
        </p>
      ))}
      <div className="manager-actions">
        <button onClick={() => action("log-stats")}>Storage statistics</button>
        <button onClick={() => action("log-compact")}>
          Archive old events
        </button>
        <button onClick={() => cliAction("log checkpoint")}>
          Build checkpoint
        </button>
        <button className="danger-quiet" onClick={() => action("hub-stop")}>
          Stop hub…
        </button>
      </div>
      {data && (
        <details className="advanced-details">
          <summary>Engine diagnostics</summary>
          <DataView value={data} />
        </details>
      )}
    </section>
  );
}
