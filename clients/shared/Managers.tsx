import { RunnerPanel } from "./RunnerPanel";
import { errorMessage } from "./errors";
import { Settings } from "./SettingsPage";
import { useHostAPI } from "./host";
import { MenuButton } from "./MenuButton";
import { scrollBehavior } from "./a11y";
import { useCallback, useEffect, useState, useRef } from "react";
import {
  Plus,
  RefreshCw,
  KeyRound,
  Server,
  Play,
  FileText,
  Trash2,
  Save,
  TerminalSquare,
} from "lucide-react";
import {
  LIVE_PARTS,
  PART_METHOD,
  focusPart,
  managePage,
  type ManagePage,
  type ManagePart,
  type Section,
} from "./settingsModel";
import type { Json } from "./types";
import { DataView, Empty, Spinner, ResourceTable } from "./ui";
import { useEditorSessions } from "./editors";

export { navSection } from "./settingsModel";
export type { Section } from "./settingsModel";

export interface ManagerProps {
  section: Section;
  workspace: string;
  refreshKey: number;
  rpc: (method: string, params?: Json) => Promise<Json>;
  action: (id: string, initial?: Json) => void;
  cliAction: (path: string, initial?: Json) => void;
  fail: (error: unknown) => void;
  canAdmin?: boolean;
  /** A live hub connection (Settings › Loops reads the hub). */
  connected?: boolean;
  /** `hub.info`'s `orchestrator.access` (absent on an older hub). */
  orchestratorAccess?: string;
  /** The orchestrator's settings changed here: read `hub.info` again. */
  infoChanged?: () => void;
}

/** A Manage page (Machines & accounts, Activity, Context & memory, Trash), or Settings:
 * Read aloud (`speech`) and Hub & storage (`maintenance`) are Settings' Voice and Advanced
 * tabs. */
export function Manager(props: ManagerProps) {
  const { section } = props;
  if (
    section === "settings" ||
    section === "speech" ||
    section === "maintenance"
  )
    return (
      <Settings
        section={section}
        fail={props.fail}
        rpc={props.rpc}
        action={props.action}
        cliAction={props.cliAction}
        canAdmin={props.canAdmin !== false}
        connected={props.connected !== false}
        refreshKey={props.refreshKey}
        orchestratorAccess={props.orchestratorAccess}
        infoChanged={props.infoChanged}
      />
    );
  const page = managePage(section);
  if (!page) return null;
  return <ManagePageView key={page.id} page={page} {...props} />;
}

const PART_TITLE: Record<ManagePart, string> = {
  accounts: "Accounts",
  boxes: "Machines",
  jobs: "Jobs",
  usage: "Usage",
  memory: "Context & memory",
  trash: "Trash",
};

function ManagePageView(props: ManagerProps & { page: ManagePage }) {
  const { page, section, rpc, action, cliAction, fail } = props;
  const [data, setData] = useState<Partial<Record<ManagePart, Json>>>({});
  const [errors, setErrors] = useState<Partial<Record<ManagePart, string>>>({});
  const [loading, setLoading] = useState(false);
  const [detail, setDetail] = useState<Json>();
  const headings = useRef<Partial<Record<ManagePart, HTMLHeadingElement>>>({});
  const loadPart = useCallback(
    async (part: ManagePart) => {
      try {
        const value = await rpc(PART_METHOD[part]);
        setData((old) => ({ ...old, [part]: value }));
        setErrors((old) => ({ ...old, [part]: "" }));
      } catch (e) {
        setErrors((old) => ({ ...old, [part]: errorMessage(e) }));
      }
    },
    [rpc],
  );
  const load = useCallback(
    async (parts: ManagePart[] = page.parts) => {
      setLoading(true);
      try {
        await Promise.all(parts.map(loadPart));
      } finally {
        setLoading(false);
      }
    },
    [page, loadPart],
  );
  useEffect(() => {
    void load();
  }, [load, props.refreshKey]);
  useEffect(() => {
    const live = page.parts.filter((p) => LIVE_PARTS.includes(p));
    if (!live.length) return;
    const timer = setInterval(() => void load(live), 5000);
    return () => clearInterval(timer);
  }, [page, load]);
  // Boxes and Usage open their page at their own part.
  useEffect(() => {
    const part = focusPart(section);
    const heading = part && headings.current[part];
    if (!heading) return;
    heading.scrollIntoView?.({ behavior: scrollBehavior(), block: "start" });
    heading.focus({ preventScroll: true });
  }, [section]);
  const inspect = async (method: string, params: Json = {}) => {
    try {
      setDetail(await rpc(method, params));
    } catch (e) {
      fail(e);
    }
  };
  const single = page.parts.length === 1;
  return (
    <div className="manager">
      <header className="section-heading">
        <div>
          <h1>{page.title}</h1>
          <p>{page.description}</p>
        </div>
        <button
          className="icon-button"
          onClick={() => void load()}
          aria-label="Refresh"
          title="Refresh"
          aria-busy={loading || undefined}
        >
          {loading ? <Spinner /> : <RefreshCw size={18} />}
        </button>
      </header>
      {page.parts.map((part) => {
        const value = data[part];
        const error = errors[part];
        const headingId = `manage-${part}`;
        return (
          <section
            key={part}
            className="manage-part"
            aria-labelledby={single ? undefined : headingId}
          >
            {!single && (
              <h2
                id={headingId}
                tabIndex={-1}
                ref={(node) => {
                  if (node) headings.current[part] = node;
                }}
              >
                {PART_TITLE[part]}
              </h2>
            )}
            {error && (
              <div className="error-banner" role="alert">
                {error}
                <button onClick={() => void load([part])}>Try again</button>
              </div>
            )}
            <PartActions part={part} action={action} cliAction={cliAction} />
            {value && (
              <PartContent
                part={part}
                data={value}
                props={props}
                reload={() => void load([part])}
                inspect={inspect}
                showLog={(job) => setDetail({ kind: "job-log", job })}
              />
            )}
          </section>
        );
      })}
      {detail !== undefined && (
        <section className="detail-result" aria-labelledby="twr-detail-title">
          <header>
            <h2 id="twr-detail-title">Details</h2>
            <button onClick={() => setDetail(undefined)}>Close</button>
          </header>
          {detail.kind === "job-log" ? (
            <JobLog job={detail.job} rpc={rpc} />
          ) : page.id === "memory" && detail.name ? (
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

/** A part's own actions: the ones that aren't on its rows. */
function PartActions({
  part,
  action,
  cliAction,
}: {
  part: ManagePart;
  action: ManagerProps["action"];
  cliAction: ManagerProps["cliAction"];
}) {
  let buttons;
  if (part === "accounts")
    buttons = (
      <>
        <button className="primary" onClick={() => cliAction("account add")}>
          <Plus size={15} /> Add account
        </button>
        <button onClick={() => action("account-check")}>Check logins</button>
      </>
    );
  else if (part === "boxes")
    buttons = (
      <>
        <button className="primary" onClick={() => cliAction("box add")}>
          <Plus size={15} /> Add machine
        </button>
        <button onClick={() => cliAction("box project")}>
          Connect a project
        </button>
      </>
    );
  else if (part === "jobs")
    buttons = (
      <>
        <button className="primary" onClick={() => action("job")}>
          <Play size={15} /> Run a job
        </button>
        <button onClick={() => action("shell")}>
          <TerminalSquare size={15} /> Shell command
        </button>
      </>
    );
  else if (part === "usage")
    buttons = (
      <button onClick={() => action("usage-threads")}>
        Usage by thread, project or runtime
      </button>
    );
  else if (part === "memory")
    buttons = (
      <>
        <button className="primary" onClick={() => action("memory-save")}>
          <Plus size={15} /> New note
        </button>
        <button onClick={() => action("context-read")}>
          <FileText size={15} /> Standing context
        </button>
      </>
    );
  else
    buttons = (
      <button className="danger-quiet" onClick={() => action("purge")}>
        <Trash2 size={15} /> Empty trash…
      </button>
    );
  return <div className="manager-actions">{buttons}</div>;
}

function PartContent({
  part,
  data,
  props,
  reload,
  inspect,
  showLog,
}: {
  part: ManagePart;
  data: Json;
  props: ManagerProps;
  reload: () => void;
  inspect: (method: string, params?: Json) => Promise<void>;
  showLog: (job: string) => void;
}) {
  const { rpc, action, cliAction } = props;
  if (part === "accounts")
    return (
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
    );
  if (part === "boxes")
    return (
      <div className="cards">
        {props.canAdmin !== false && <RunnerPanel rpc={rpc} refresh={reload} />}
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
                  <b>{a.found ? (a.login === "logged_in" ? "✓" : "?") : "—"}</b>
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
              <MenuButton
                label={`More actions for ${box.name}`}
                items={[
                  {
                    label: "Install or update twr",
                    onSelect: () =>
                      cliAction("box install", { name: box.name }),
                  },
                  {
                    label: "Restart…",
                    onSelect: () =>
                      cliAction("box restart", { name: box.name }),
                  },
                  {
                    label: "Projects",
                    onSelect: () =>
                      void inspect("box.projects", { box: box.name }),
                  },
                  {
                    label: "Token…",
                    onSelect: () => cliAction("box token", { name: box.name }),
                  },
                  {
                    label: "Remove…",
                    danger: true,
                    onSelect: () => cliAction("box remove", { name: box.name }),
                  },
                ]}
              />
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
    );
  if (part === "usage")
    return (
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
                  <small>Resets {new Date(w.resets_at).toLocaleString()}</small>
                )}
              </div>
            ))}
          </article>
        ))}
        {!data.runtimes?.length && (
          <p className="muted">
            Your agents have not reported subscription limits yet. Usage appears
            after a session reports it.
          </p>
        )}
      </div>
    );
  if (part === "memory")
    return (
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
                onClick={() => void inspect("memory.read", { name: note.name })}
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
    );
  if (part === "jobs")
    return (
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
            <button onClick={() => showLog(j.job)}>Output</button>
            <button onClick={() => action("job-cancel", { job: j.job })}>
              Cancel…
            </button>
          </>
        )}
      />
    );
  return (
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
              onClick={() => action("purge", { items: t.item, dry_run: true })}
            >
              Purge…
            </button>
          </>
        )}
      />
      {data.message && <p className="form-note">{data.message}</p>}
      {data.boxes_offline && <DataView value={data.boxes_offline} />}
    </>
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
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <pre
        ref={output}
        aria-label={`Job output ${job.slice(-8)}`}
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
