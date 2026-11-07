import { OfflineOptions } from "./OfflineOptions";
import { LayoutOptions } from "./LayoutOptions";
import { errorMessage } from "./errors";
import { AppearanceOptions } from "./AppearanceOptions";
import { useHostAPI } from "./host";
import { useCallback, useEffect, useState, useRef } from "react";
import {
  Plus,
  RefreshCw,
  ArrowUpRight,
  KeyRound,
  Server,
  Play,
  FileText,
  Trash2,
  Save,
  FolderOpen,
  Volume2,
  TerminalSquare,
  Settings2,
  Download,
} from "lucide-react";
import { numberSetting } from "./settings";
import type { Json } from "./types";
import {
  DataView,
  Empty,
  Spinner,
  formatNumber,
  Markdown,
  ResourceTable,
} from "./ui";
import { useEditorSessions } from "./editors";
export type Section =
  | "chat"
  | "grid"
  | "attention"
  | "accounts"
  | "boxes"
  | "jobs"
  | "usage"
  | "memory"
  | "trash"
  | "settings"
  | "maintenance"
  | "decisions"
  | "speech";
export interface ManagerProps {
  section: Section;
  workspace: string;
  refreshKey: number;
  rpc: (method: string, params?: Json) => Promise<Json>;
  action: (id: string, initial?: Json) => void;
  cliAction: (path: string, initial?: Json) => void;
  fail: (error: unknown) => void;
  canAdmin?: boolean;
}
const meta: Record<
  string,
  { title: string; description: string; method: string }
> = {
  accounts: {
    title: "Accounts",
    description: "Your subscriptions, connected to your agents.",
    method: "account.list",
  },
  boxes: {
    title: "Boxes",
    description: "The machines where your agents work.",
    method: "hub.info",
  },
  jobs: {
    title: "Jobs",
    description: "Commands, background work, and their output.",
    method: "job.list",
  },
  usage: {
    title: "Usage",
    description: "Subscription limits and the work your threads have done.",
    method: "usage.get",
  },
  memory: {
    title: "Context & memory",
    description: "What your orchestrator knows between conversations.",
    method: "memory.list",
  },
  trash: {
    title: "Trash",
    description:
      "Deleted threads stay recoverable until their retention period ends.",
    method: "trash.list",
  },
  maintenance: {
    title: "Hub & storage",
    description: "The engine keeps running when you close this window.",
    method: "hub.info",
  },
  speech: {
    title: "Read aloud",
    description: "Listen through a speaker connected to your hub.",
    method: "speech.status",
  },
};
export function Manager(props: ManagerProps) {
  const hostApi = useHostAPI();
  const { section, rpc, action, cliAction, fail } = props;
  const info = meta[section];
  const [data, setData] = useState<Json>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [detail, setDetail] = useState<Json>();
  const load = useCallback(async () => {
    if (!info) return;
    setLoading(true);
    setError("");
    try {
      setData(await rpc(info.method));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [info, rpc]);
  useEffect(() => {
    void load();
  }, [load, props.refreshKey]);
  useEffect(() => {
    if (!["jobs", "boxes", "usage", "speech", "trash"].includes(section))
      return;
    const timer = setInterval(() => void load(), 5000);
    return () => clearInterval(timer);
  }, [section, load]);
  if (section === "settings")
    return (
      <Settings
        fail={fail}
        rpc={rpc}
        action={action}
        canAdmin={props.canAdmin !== false}
      />
    );
  if (!info) return null;
  const inspect = async (method: string, params: Json = {}) => {
    try {
      setDetail(await rpc(method, params));
    } catch (e) {
      fail(e);
    }
  };
  return (
    <div className="manager">
      <header className="section-heading">
        <div>
          <span className="eyebrow">Workspace</span>
          <h1>{info.title}</h1>
          <p>{info.description}</p>
        </div>
        <button
          className="icon-button"
          onClick={() => void load()}
          aria-label="Refresh"
        >
          {loading ? <Spinner /> : <RefreshCw size={18} />}
        </button>
      </header>
      {error && (
        <div className="error-banner">
          {error}
          <button onClick={() => void load()}>Try again</button>
        </div>
      )}
      <div className="manager-actions">
        {section === "accounts" && (
          <>
            <button
              className="primary"
              onClick={() => cliAction("account add")}
            >
              <Plus size={15} /> Add account
            </button>
            <button onClick={() => cliAction("account login")}>
              <KeyRound size={15} /> Sign in
            </button>
            <button onClick={() => action("account-check")}>
              Check logins
            </button>
          </>
        )}
        {section === "boxes" && (
          <>
            <button className="primary" onClick={() => cliAction("box add")}>
              <Plus size={15} /> Add box
            </button>
            <button onClick={() => cliAction("box project")}>
              Connect a project
            </button>
            <button onClick={() => cliAction("box doctor")}>
              Diagnose a box
            </button>
          </>
        )}
        {section === "jobs" && (
          <>
            <button className="primary" onClick={() => action("job")}>
              <Play size={15} /> Run a job
            </button>
            <button onClick={() => action("shell")}>
              <TerminalSquare size={15} /> Shell command
            </button>
          </>
        )}
        {section === "usage" && (
          <button onClick={() => action("usage-threads")}>
            Usage by thread / project / runtime
          </button>
        )}
        {section === "memory" && (
          <>
            <button className="primary" onClick={() => action("memory-save")}>
              <Plus size={15} /> New note
            </button>
            <button onClick={() => action("context-read")}>
              <FileText size={15} /> Standing context
            </button>
          </>
        )}
        {section === "trash" && (
          <>
            <button onClick={() => action("restore")}>Restore a thread</button>
            <button className="danger-quiet" onClick={() => action("purge")}>
              <Trash2 size={15} /> Empty trash…
            </button>
          </>
        )}
        {section === "maintenance" && (
          <>
            <button onClick={() => action("app-update")}>
              <Download size={14} />
              Check for updates
            </button>
            <button onClick={() => action("log-stats")}>
              Storage statistics
            </button>
            <button onClick={() => action("log-compact")}>
              Archive old events
            </button>
            <button onClick={() => cliAction("log checkpoint")}>
              Build checkpoint
            </button>
            <button className="danger-quiet" onClick={() => action("hub-stop")}>
              Stop hub…
            </button>
          </>
        )}
        {section === "speech" && (
          <>
            <button className="primary" onClick={() => cliAction("speak")}>
              <Volume2 size={15} /> Start a speaker
            </button>
            <button onClick={() => action("read")}>Read messages</button>
            <button onClick={() => action("speech-control")}>
              Playback settings
            </button>
          </>
        )}
      </div>
      {section === "accounts" && data && (
        <div className="cards">
          {Object.entries(data.runtimes || {}).flatMap(
            ([runtime, group]: [string, any]) =>
              (group.accounts || []).map((a: Json) => (
                <article className="resource-card" key={runtime + ":" + a.name}>
                  <div className="card-heading">
                    <KeyRound size={18} />
                    <h3>{a.name}</h3>
                    <span
                      className={`pill ${a.login === "logged_in" ? "success" : ""}`}
                    >
                      {a.removed
                        ? "Removed"
                        : a.login?.replaceAll("_", " ") || "Not checked"}
                    </span>
                  </div>
                  <p>
                    {runtime} {a.native ? "· Native login" : ""}
                  </p>
                  {a.conflict && <p className="error-text">{a.conflict}</p>}
                  <div className="card-actions">
                    <button
                      onClick={() =>
                        cliAction("account login", { runtime, name: a.name })
                      }
                    >
                      Sign in
                    </button>
                    <button
                      onClick={() =>
                        action("account-check", { runtime, account: a.name })
                      }
                    >
                      Check
                    </button>
                    <button
                      onClick={() =>
                        action("account-remove", { runtime, account: a.name })
                      }
                    >
                      Remove…
                    </button>
                  </div>
                </article>
              )),
          )}
        </div>
      )}
      {section === "boxes" && data && (
        <div className="cards">
          {(data.boxes || []).map((box: Json) => (
            <article className="resource-card" key={box.name}>
              <div className="card-heading">
                <Server size={18} />
                <h3>{box.name}</h3>
                <span className={`pill ${box.online ? "success" : ""}`}>
                  {box.state}
                </span>
              </div>
              <p>
                {box.transport} · {box.os || "OS unknown"} {box.arch} · twr{" "}
                {box.version || "unknown"}
              </p>
              {box.last_error && <p className="error-text">{box.last_error}</p>}
              <div className="agents">
                {box.agents?.map((a: Json) => (
                  <span key={a.runtime}>
                    {a.runtime}{" "}
                    <b>
                      {a.found ? (a.login === "logged_in" ? "✓" : "?") : "—"}
                    </b>
                  </span>
                ))}
              </div>
              <div className="card-actions">
                <button
                  onClick={() => cliAction("box doctor", { name: box.name })}
                >
                  Diagnose
                </button>
                <button
                  onClick={() => cliAction("box login", { name: box.name })}
                >
                  Sign in
                </button>
                <button
                  onClick={() => cliAction("box install", { name: box.name })}
                >
                  Install / update
                </button>
                <button
                  onClick={() => cliAction("box restart", { name: box.name })}
                >
                  Restart…
                </button>
                <button
                  onClick={() => cliAction("box remove", { name: box.name })}
                >
                  Remove…
                </button>
                <button
                  onClick={() => inspect("box.projects", { box: box.name })}
                >
                  Projects
                </button>
                <button
                  onClick={() => cliAction("box token", { name: box.name })}
                >
                  Token…
                </button>
              </div>
            </article>
          ))}
          {!data.boxes?.length && (
            <Empty
              title="Work on another machine"
              description="Connect a machine over SSH, a container command, or a runner connection. Your existing agents and git credentials stay on that machine."
            />
          )}
        </div>
      )}
      {section === "usage" && data && (
        <div className="cards">
          {(data.runtimes || []).map((runtime: Json) => (
            <article
              className="resource-card"
              key={runtime.runtime + ":" + runtime.account}
            >
              <div className="card-heading">
                <h3>{runtime.label || runtime.runtime}</h3>
                {runtime.stale && <span className="pill">Out of date</span>}
              </div>
              <p>
                {runtime.plan || "Subscription"}
                {runtime.account ? " · " + runtime.account : ""}
              </p>
              {runtime.windows.map((w: Json) => (
                <div className="usage-window" key={w.window}>
                  <div>
                    <span>{w.label}</span>
                    <b>
                      {Math.round(
                        w.remaining_pct ?? Math.max(0, 100 - w.used_pct),
                      )}
                      % left
                    </b>
                  </div>
                  <meter
                    min={0}
                    max={100}
                    value={Math.max(0, Math.min(100, w.used_pct))}
                  />
                  {w.resets_at && (
                    <small>
                      Resets {new Date(w.resets_at).toLocaleString()}
                    </small>
                  )}
                </div>
              ))}
            </article>
          ))}
          {!data.runtimes?.length && (
            <p className="muted">
              Your agents have not reported subscription limits yet. Usage
              appears after a session reports it.
            </p>
          )}
        </div>
      )}
      {section === "memory" && data && (
        <div className="cards">
          {(data.notes || []).map((note: Json) => (
            <article className="resource-card" key={note.name}>
              <div className="card-heading">
                <FileText size={18} />
                <h3>{note.name}</h3>
                <span className="pill">{note.type}</span>
              </div>
              <p>{note.description}</p>
              <div className="card-actions">
                <button
                  onClick={() => inspect("memory.read", { name: note.name })}
                >
                  Read / edit
                </button>
                <button
                  onClick={() =>
                    action("memory-forget", { name: note.name, rev: note.rev })
                  }
                >
                  Forget…
                </button>
              </div>
            </article>
          ))}
        </div>
      )}
      {section === "jobs" && data && (
        <ResourceTable
          rows={Array.isArray(data) ? data : data.jobs || []}
          empty="No jobs are running or queued."
          columns={[
            {
              key: "name",
              label: "Job",
              render: (j) => (
                <div>
                  <b>{j.name}</b>
                  <small className="table-subline">{j.argv?.join(" ")}</small>
                </div>
              ),
            },
            { key: "thread_name", label: "Thread" },
            {
              key: "state",
              label: "State",
              render: (j) => (
                <span className="pill">
                  {j.state}
                  {j.position != null ? " · position " + (j.position + 1) : ""}
                </span>
              ),
            },
            {
              key: "running_secs",
              label: "Elapsed",
              render: (j) =>
                j.running_secs != null
                  ? Math.floor(j.running_secs / 60) +
                    "m " +
                    (j.running_secs % 60) +
                    "s"
                  : "Queued",
            },
          ]}
          actions={(j) => (
            <>
              <button
                onClick={() => setDetail({ kind: "job-log", job: j.job })}
              >
                Output
              </button>
              <button onClick={() => action("job-cancel", { job: j.job })}>
                Cancel…
              </button>
            </>
          )}
        />
      )}
      {section === "trash" && data && (
        <>
          <ResourceTable
            rows={data.items || []}
            empty="The trash is empty."
            columns={[
              {
                key: "thread_name",
                label: "Thread",
                render: (t) => (
                  <div>
                    <b>{t.thread_name}</b>
                    <small className="table-subline">
                      {t.project} · {t.box}
                    </small>
                  </div>
                ),
              },
              { key: "size", label: "Size" },
              {
                key: "deleted_at",
                label: "Deleted",
                render: (t) => new Date(t.deleted_at).toLocaleString(),
              },
              {
                key: "purge_at",
                label: "Purges",
                render: (t) => new Date(t.purge_at).toLocaleDateString(),
              },
              { key: "losses", label: "Saved changes" },
            ]}
            actions={(t) => (
              <>
                {t.incomplete && (
                  <span className="form-note">
                    Deletion is incomplete; review the thread before restoring.
                  </span>
                )}
                <button
                  disabled={!!t.incomplete}
                  onClick={() => action("restore", { item: t.item })}
                >
                  Restore
                </button>
                <button
                  className="danger-quiet"
                  disabled={!!t.incomplete}
                  onClick={() =>
                    action("purge", { items: t.item, dry_run: true })
                  }
                >
                  Purge…
                </button>
              </>
            )}
          />
          {data.message && <p className="form-note">{data.message}</p>}
          {data.boxes_offline && <DataView value={data.boxes_offline} />}
        </>
      )}
      {section === "maintenance" && data && (
        <>
          <div className="cards">
            <article className="resource-card">
              <h3>Engine</h3>
              <p>
                twr {data.version} · {data.workspace}
              </p>
              <p>
                {data.threads} threads · {data.open_attention} items need you
              </p>
              <small>
                Running since {new Date(data.started_at).toLocaleString()}
              </small>
            </article>
            <article className="resource-card">
              <h3>Storage</h3>
              <p className="path">{data.db}</p>
              <button onClick={() => void hostApi.reveal(data.db).catch(fail)}>
                Show database folder <FolderOpen size={13} />
              </button>
            </article>
          </div>
          {data.warnings?.map((w: string) => (
            <p className="error-banner" key={w}>
              {w}
            </p>
          ))}
          <details className="advanced-details">
            <summary>Engine diagnostics</summary>
            <DataView value={data} />
          </details>
        </>
      )}
      {section === "speech" && data && (
        <>
          <ResourceTable
            rows={data.speakers || []}
            empty="No speakers are connected. Start one to listen on this machine."
            columns={[
              { key: "name", label: "Speaker" },
              { key: "engine", label: "Engine" },
              { key: "voice", label: "Voice" },
              { key: "rate", label: "Words/min" },
              {
                key: "paused",
                label: "Playback",
                render: (s) => (s.paused ? "Paused" : "Ready"),
              },
            ]}
            actions={(speaker) => (
              <>
                <button
                  onClick={() =>
                    rpc("speech.control", {
                      op: "pause",
                      speaker: speaker.name,
                    })
                      .then(() => load())
                      .catch(fail)
                  }
                >
                  Pause
                </button>
                <button
                  onClick={() =>
                    rpc("speech.control", {
                      op: "resume",
                      speaker: speaker.name,
                    })
                      .then(() => load())
                      .catch(fail)
                  }
                >
                  Resume
                </button>
                <button
                  onClick={() =>
                    rpc("speech.control", { op: "stop", speaker: speaker.name })
                      .then(() => load())
                      .catch(fail)
                  }
                >
                  Stop
                </button>
              </>
            )}
          />
          <details className="advanced-details">
            <summary>Speaker diagnostics</summary>
            <DataView value={data} />
          </details>
        </>
      )}
      {detail !== undefined && (
        <section className="detail-result">
          <header>
            <h2>Details</h2>
            <button onClick={() => setDetail(undefined)}>Close</button>
          </header>
          {detail.kind === "job-log" ? (
            <JobLog job={detail.job} rpc={rpc} />
          ) : section === "memory" && detail.name ? (
            <MemoryEditor
              key={props.workspace + ":" + detail.name + ":" + detail.rev}
              scope={props.workspace}
              note={detail}
              rpc={rpc}
              fail={fail}
              saved={() => {
                setDetail(undefined);
                void load();
              }}
            />
          ) : (
            <DataView value={detail} />
          )}
        </section>
      )}
    </div>
  );
}
function MemoryEditor({
  note,
  rpc,
  fail,
  saved,
  scope,
}: {
  note: Json;
  rpc: ManagerProps["rpc"];
  fail: ManagerProps["fail"];
  saved: () => void;
  scope: string;
}) {
  const value = note.note || note;
  const editors = useEditorSessions();
  const key = "memory:" + scope + ":" + value.name;
  const existing = editors.get(key);
  const revision = existing?.rev ?? value.rev;
  const [body, setBody] = useState(existing?.body ?? value.body ?? "");
  const [description, setDescription] = useState(
    existing?.description ?? value.description ?? "",
  );
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="editor-form"
      onSubmit={(e) => {
        e.preventDefault();
        setBusy(true);
        void rpc("memory.save", {
          name: value.name,
          type: value.type,
          description,
          body,
          rev: revision,
        })
          .then(() => {
            editors.clear(key);
            saved();
          })
          .catch(fail)
          .finally(() => setBusy(false));
      }}
    >
      <label className="field">
        Description
        <input
          aria-label="Description"
          value={description}
          onChange={(e) => {
            setDescription(e.target.value);
            editors.set(key, {
              body,
              description: e.target.value,
              rev: revision,
            });
          }}
        />
      </label>
      <label className="field">
        Note
        <textarea
          aria-label="Note"
          rows={14}
          value={body}
          onChange={(e) => {
            setBody(e.target.value);
            editors.set(key, {
              body: e.target.value,
              description,
              rev: revision,
            });
          }}
        />
      </label>
      <button className="primary" disabled={busy}>
        {busy ? <Spinner /> : <Save size={15} />} Save note
      </button>
    </form>
  );
}
function Settings({
  fail,
  rpc,
  action,
  canAdmin,
}: {
  canAdmin: boolean;
  fail: ManagerProps["fail"];
  rpc: ManagerProps["rpc"];
  action: ManagerProps["action"];
}) {
  const hostApi = useHostAPI();
  const editors = useEditorSessions();
  const [config, setConfig] = useState<{
    content: string;
    hash: string;
    path: string;
    settings?: Json;
  }>();
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [loaded, setLoaded] = useState<Json>();
  useEffect(() => {
    if (!canAdmin) return;
    void hostApi
      .configRead()
      .then((c) => {
        setConfig(c);
        setText(editors.get("config")?.text ?? c.content);
      })
      .catch(fail);
    void rpc("config.show").then(setLoaded).catch(fail);
  }, [rpc, fail, canAdmin]);
  useEffect(() => {
    if (!config || text === config.content) return;
    const listener = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", listener);
    return () => window.removeEventListener("beforeunload", listener);
  }, [text, config]);
  if (!canAdmin)
    return (
      <div className="manager">
        <header className="section-heading">
          <div>
            <h1>Settings</h1>
            <p>Preferences for this device.</p>
          </div>
        </header>
        <ClientPreferences />
      </div>
    );
  return (
    <div className="manager">
      <header className="section-heading">
        <div>
          <span className="eyebrow">Preferences</span>
          <h1>Settings</h1>
          <p>Make twr feel at home.</p>
        </div>
      </header>
      <ClientPreferences />
      <section className="settings-section">
        <h2>Orchestrator</h2>
        <p>
          Switch runtime and model with a handoff, or start a fresh
          conversation.
        </p>
        <div className="manager-actions">
          <button onClick={() => action("orch")}>Change orchestrator</button>
          <button onClick={() => action("clear")}>Fresh conversation…</button>
        </div>
      </section>
      <section className="settings-section">
        <h2>Session defaults</h2>
        <p>
          Save individual defaults without changing the rest of the file.
          Restart the hub to apply new hub defaults.
        </p>
        {loaded && (
          <SettingControls
            loaded={config?.settings || loaded}
            fail={fail}
            saved={() => {
              void hostApi
                .configRead()
                .then((c) => {
                  setConfig(c);
                  if (!editors.get("config")) setText(c.content);
                })
                .catch(fail);
            }}
          />
        )}
      </section>
      <section className="settings-section">
        <h2>Configuration</h2>
        <p>
          All CLI configuration is available here. Comments and formatting are
          preserved. Hub changes apply after a hub restart.
        </p>
        {config && (
          <>
            <div className="config-path">
              <code>{config.path}</code>
              <button
                className="icon-button"
                aria-label="Show configuration file"
                onClick={() => void hostApi.reveal(config.path)}
              >
                <FolderOpen size={16} />
              </button>
            </div>
            <textarea
              className="config-editor"
              aria-label="Configuration TOML"
              value={text}
              onChange={(e) => {
                setText(e.target.value);
                setSaved(false);
                editors.set("config", {
                  text: e.target.value,
                  hash: editors.get("config")?.hash ?? config.hash,
                });
              }}
              spellCheck={false}
              rows={18}
            />
            <div className="manager-actions">
              <button
                className="primary"
                disabled={busy || text === config.content}
                onClick={() => {
                  setBusy(true);
                  void hostApi
                    .configSave(
                      text,
                      editors.get("config")?.hash ?? config.hash,
                    )
                    .then(() => hostApi.configRead())
                    .then((c) => {
                      editors.clear("config");
                      setConfig(c);
                      setText(c.content);
                      setSaved(true);
                    })
                    .catch(fail)
                    .finally(() => setBusy(false));
                }}
              >
                {busy ? <Spinner /> : <Save size={15} />}Save configuration
              </button>
              <button
                onClick={() => {
                  if (
                    text !== config.content &&
                    !window.confirm("Discard your unsaved configuration edits?")
                  )
                    return;
                  void hostApi
                    .configRead()
                    .then((c) => {
                      editors.clear("config");
                      setConfig(c);
                      setText(c.content);
                    })
                    .catch(fail);
                }}
              >
                Reload from disk
              </button>
              {saved && (
                <span className="success-text">
                  Saved. Restart the hub to apply hub settings.
                </span>
              )}
            </div>
          </>
        )}
      </section>
      {loaded && (
        <section className="settings-section">
          <h2>Currently loaded configuration</h2>
          <p>
            These are the effective values the running hub uses. Credentials are
            omitted.
          </p>
          <details>
            <summary>Show effective configuration</summary>
            <DataView value={loaded} />
          </details>
        </section>
      )}
    </div>
  );
}

export function JobLog({
  job,
  rpc,
}: {
  job: string;
  rpc: ManagerProps["rpc"];
}) {
  const hostApi = useHostAPI();
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const output = useRef<HTMLPreElement>(null);
  const follow = useRef(true);
  useEffect(() => {
    if (follow.current && output.current)
      output.current.scrollTop = output.current.scrollHeight;
  }, [text]);
  useEffect(() => {
    let active = true;
    const load = () =>
      void rpc("job.log", { job, lines: 2000 })
        .then((value) => {
          if (active) {
            setText(value.text || "");
            setError("");
          }
        })
        .catch((e) => {
          if (active) setError(errorMessage(e));
        });
    load();
    const timer = setInterval(load, 1500);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, [job, rpc]);
  return (
    <div className="job-log">
      <div className="manager-actions">
        <span className="muted">Live output · {job.slice(-8)}</span>
        <button
          type="button"
          onClick={() =>
            void hostApi.exportText(`job-${job.slice(-8)}.log`, text)
          }
        >
          Export log
        </button>
      </div>
      {error && <p className="error-text">{error}</p>}
      <pre
        ref={output}
        onScroll={() => {
          const p = output.current!;
          follow.current = p.scrollHeight - p.scrollTop - p.clientHeight < 48;
        }}
        tabIndex={0}
      >
        {text || "Waiting for output…"}
      </pre>
    </div>
  );
}
const scalarSettings: {
  table: string[];
  key: string;
  label: string;
  kind: "string" | "number" | "boolean";
  options?: string[];
}[] = [
  {
    table: ["tui"],
    key: "card_order",
    label: "Thread card order",
    kind: "string",
    options: ["stable", "attention"],
  },
  {
    table: [],
    key: "default_runtime",
    label: "Default runtime",
    kind: "string",
  },
  {
    table: ["threads"],
    key: "default_mode",
    label: "Default permission mode",
    kind: "string",
    options: [
      "auto",
      "plan",
      "manual",
      "acceptEdits",
      "read-only",
      "dontAsk",
      "bypassPermissions",
    ],
  },
  {
    table: ["threads"],
    key: "worktree",
    label: "Separate worktrees",
    kind: "boolean",
  },
  { table: ["threads"], key: "model", label: "Default model", kind: "string" },
  {
    table: ["threads"],
    key: "thinking",
    label: "Include reasoning events",
    kind: "boolean",
  },
  {
    table: ["threads"],
    key: "idle_compaction",
    label: "Automatic context compaction",
    kind: "string",
    options: ["after_expiry", "before_expiry", "off"],
  },
  {
    table: [],
    key: "stall_after_secs",
    label: "Stall notification after (seconds)",
    kind: "number",
  },
  {
    table: [],
    key: "background_timeout_secs",
    label: "Background timeout (seconds)",
    kind: "number",
  },
  {
    table: ["trash"],
    key: "keep_days",
    label: "Keep deleted threads (days)",
    kind: "number",
  },
  {
    table: ["log"],
    key: "retention_days",
    label: "Keep full agent logs (days)",
    kind: "number",
  },
  {
    table: ["log"],
    key: "archive",
    label: "Archive old logs",
    kind: "boolean",
  },
  {
    table: ["tui"],
    key: "summaries",
    label: "Summarize routine thread updates",
    kind: "boolean",
  },
  {
    table: ["tui"],
    key: "summary_every",
    label: "Summary interval (for example 5m)",
    kind: "string",
  },
  {
    table: ["tui"],
    key: "suggest",
    label: "Suggest next messages",
    kind: "boolean",
  },
];
function SettingControls({
  loaded,
  fail,
  saved,
}: {
  loaded: Json;
  fail: ManagerProps["fail"];
  saved: () => void;
}) {
  return (
    <div className="setting-controls">
      {scalarSettings.map((setting) => {
        let value = loaded;
        for (const table of setting.table) value = value?.[table];
        return (
          <ScalarSetting
            key={setting.table.join(".") + setting.key}
            setting={setting}
            initial={value?.[setting.key]}
            fail={fail}
            saved={saved}
          />
        );
      })}
    </div>
  );
}
function ScalarSetting({
  setting,
  initial,
  fail,
  saved,
}: {
  setting: (typeof scalarSettings)[number];
  initial: Json;
  fail: ManagerProps["fail"];
  saved: () => void;
}) {
  const hostApi = useHostAPI();
  const [value, setValue] = useState(
    initial ?? (setting.kind === "boolean" ? false : ""),
  );
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  return (
    <form
      className="setting-row"
      onSubmit={(event) => {
        event.preventDefault();
        let next;
        try {
          next = setting.kind === "number" ? numberSetting(value) : value;
        } catch (error) {
          fail(error);
          return;
        }
        if (
          setting.table[0] === "trash" &&
          setting.key === "keep_days" &&
          next === 0 &&
          !window.confirm(
            "Permanently purge deleted threads at the next cleanup? A retention of zero days deletes their worktrees and branches. This cannot be undone.",
          )
        )
          return;
        setBusy(true);
        void hostApi
          .configSet(setting.table, setting.key, next)
          .then(() => {
            setDone(true);
            saved();
          })
          .catch(fail)
          .finally(() => setBusy(false));
      }}
    >
      <label>
        <span>{setting.label}</span>
        <small>
          {setting.table.length ? setting.table.join(".") + "." : ""}
          {setting.key}
        </small>
      </label>
      {setting.kind === "boolean" ? (
        <input
          aria-label={setting.label}
          type="checkbox"
          checked={!!value}
          onChange={(e) => {
            setValue(e.target.checked);
            setDone(false);
          }}
        />
      ) : setting.options ? (
        <select
          aria-label={setting.label}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setDone(false);
          }}
        >
          {setting.options.map((o) => (
            <option key={o}>{o}</option>
          ))}
        </select>
      ) : (
        <input
          aria-label={setting.label}
          type={setting.kind === "number" ? "number" : "text"}
          required={setting.kind === "number"}
          min={setting.kind === "number" ? 0 : undefined}
          step={setting.kind === "number" ? 1 : undefined}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setDone(false);
          }}
        />
      )}
      <button disabled={busy || done}>
        {busy ? <Spinner /> : done ? "Saved" : "Save"}
      </button>
    </form>
  );
}

function ClientPreferences() {
  return (
    <>
      <section className="settings-section">
        <h2 id="settings-appearance-title">Appearance</h2>
        <p>
          {document.documentElement.dataset.mobile === "true"
            ? "System follows your device."
            : "System follows your desktop. On Omarchy, its current theme is used when available."}
        </p>
        <AppearanceOptions labelledBy="settings-appearance-title" />
      </section>
      <LayoutOptions />
      <OfflineOptions />
    </>
  );
}
