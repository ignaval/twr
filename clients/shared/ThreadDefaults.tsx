import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useHostAPI } from "./host";
import { durationChoices, permissionChoices, type Choice } from "./labels";
import { modelChoices } from "./models";
import {
  BACKGROUND_TIMEOUT_PRESETS,
  CARD_ORDER_CHOICES,
  CODEX_AUTO_CHOICES,
  COMPACTION_CHOICES,
  LOG_DAYS_PRESETS,
  STALL_PRESETS,
  TRASH_DAYS_PRESETS,
  WAKE_AFTER_PRESETS,
  hubApplied,
  latestWrites,
  runtimeChoices,
  runtimeInstalled,
  summaryEveryChoices,
  withCurrent,
} from "./settingsModel";
import { Toggle } from "./Toggle";
import type { Json } from "./types";
import { ModelSelect } from "./ui";

type Fail = (error: unknown) => void;

/** What a setting's row needs to save itself: `saved` reads the config file again. */
export interface SaveContext {
  fail: Fail;
  saved: () => void;
}

/** The value at `table.key` of the loaded settings. */
export function settingValue(loaded: Json, table: string[], key: string) {
  let value = loaded;
  for (const t of table) value = value?.[t];
  return value?.[key];
}

type SettingValue = string | number | boolean | string[];

/** Saving one setting on change: the brief "Saved" its row says, and whether a change of
 * a hub setting waits for a restart. Writes go one at a time, the latest change winning
 * (`latestWrites`), and the file is read again once the last one ends. `save` gives back
 * `true` once the value is saved, `false` when it failed (the failure goes to `fail`; the
 * row then shows what the file has), and `undefined` when a newer change replaced it. */
function useSave(table: string[], key: string, context: SaveContext) {
  const hostApi = useHostAPI();
  const [status, setStatus] = useState("");
  const [restart, setRestart] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const latest = useRef({ hostApi, table, context });
  latest.current = { hostApi, table, context };
  const [write] = useState(() =>
    latestWrites<SettingValue>(
      (value) =>
        latest.current.hostApi.configSet(latest.current.table, key, value),
      () => latest.current.context.saved(),
    ),
  );
  const save = async (value: SettingValue) => {
    clearTimeout(timer.current);
    setStatus("Saving…");
    const outcome = await write(value);
    if (outcome.status === "superseded") return undefined;
    if (outcome.status === "failed") {
      setStatus("");
      latest.current.context.fail(outcome.error);
      return false;
    }
    setStatus("Saved");
    if (hubApplied(table)) setRestart(true);
    timer.current = setTimeout(() => setStatus(""), 2500);
    return true;
  };
  return { save, status, restart };
}

/** The latest value of `value` (a prop), read when a write ends. */
function useLatest<T>(value: T) {
  const ref = useRef(value);
  ref.current = value;
  return ref;
}

/** A row's status: "Saved" for a moment (announced), and, once a hub setting changed, that
 * it applies after the hub restarts (read with the control). */
function Feedback({
  status,
  restart,
  noteId,
}: {
  status: string;
  restart: boolean;
  noteId: string;
}) {
  return (
    <div className="setting-feedback">
      <span role="status">{status}</span>
      {restart && (
        <small id={noteId} className="setting-note">
          Applies after the hub restarts.
        </small>
      )}
    </div>
  );
}

/** An on/off setting, saved when it is switched. */
export function SettingSwitch({
  table,
  name,
  label,
  detail,
  initial,
  context,
  onSaved,
}: {
  table: string[];
  name: string;
  label: string;
  detail?: string;
  initial: Json;
  context: SaveContext;
  onSaved?: (value: boolean) => void;
}) {
  const noteId = useId();
  const [value, setValue] = useState(!!initial);
  useEffect(() => setValue(!!initial), [initial]);
  const file = useLatest(initial);
  const { save, status, restart } = useSave(table, name, context);
  return (
    <Toggle
      label={label}
      detail={detail}
      checked={value}
      title={[...table, name].join(".")}
      describedBy={restart ? noteId : undefined}
      onChange={(next) => {
        setValue(next);
        void save(next).then((ok) => {
          if (ok) onSaved?.(next);
          // Failed: what the file has (read again, should an earlier change have saved).
          else if (ok === false) setValue(!!file.current);
        });
      }}
    >
      <Feedback status={status} restart={restart} noteId={noteId} />
    </Toggle>
  );
}

/** A labelled row with its control and status. */
function Row({
  id,
  label,
  hint,
  title,
  children,
  feedback,
}: {
  id: string;
  label: string;
  hint?: string;
  title: string;
  children: ReactNode;
  feedback: ReactNode;
}) {
  return (
    <div className="setting-field" title={title}>
      <label htmlFor={id}>
        <span>{label}</span>
        {hint && <small id={`${id}-hint`}>{hint}</small>}
      </label>
      {children}
      {feedback}
    </div>
  );
}

/** A setting chosen from a list, saved when it changes. `number`: the value is saved as
 * a number (durations). `confirm`: asked before saving a value (false keeps the old). */
export function SettingSelect({
  table,
  name,
  label,
  hint,
  initial,
  choices,
  number = false,
  confirm,
  context,
  onSaved,
}: {
  table: string[];
  name: string;
  label: string;
  hint?: string;
  initial: Json;
  choices: Choice[];
  number?: boolean;
  confirm?: (value: string) => boolean;
  context: SaveContext;
  onSaved?: (value: string) => void;
}) {
  const id = useId();
  const noteId = `${id}-note`;
  const shown = initial == null ? "" : String(initial);
  const [value, setValue] = useState(shown);
  useEffect(() => setValue(shown), [shown]);
  const file = useLatest(shown);
  const { save, status, restart } = useSave(table, name, context);
  const list =
    value && !choices.some((c) => c.value === value)
      ? [...choices, { value, label: value }]
      : choices;
  return (
    <Row
      id={id}
      label={label}
      hint={hint}
      title={[...table, name].join(".")}
      feedback={<Feedback status={status} restart={restart} noteId={noteId} />}
    >
      <select
        id={id}
        value={value}
        aria-describedby={
          [hint ? `${id}-hint` : "", restart ? noteId : ""]
            .filter(Boolean)
            .join(" ") || undefined
        }
        onChange={(e) => {
          const next = e.target.value;
          if (confirm && !confirm(next)) return;
          setValue(next);
          void save(number ? Number(next) : next).then((ok) => {
            if (ok) onSaved?.(next);
            else if (ok === false) setValue(file.current);
          });
        }}
      >
        {!value && <option value="">Not set</option>}
        {list.map((c) => (
          <option key={c.value} value={c.value}>
            {c.label}
          </option>
        ))}
      </select>
    </Row>
  );
}

/** A setting typed in (a model id, or a runtime when the hub lists none): a choice from
 * the list saves at once, typed text when the field is left or Enter is pressed. */
function SettingText({
  table,
  name,
  label,
  initial,
  models,
  context,
  onSaved,
}: {
  table: string[];
  name: string;
  label: string;
  initial: Json;
  /** The model picker's choices; absent: a plain text field. */
  models?: { value: string; description?: string }[];
  context: SaveContext;
  onSaved?: (value: string) => void;
}) {
  const id = useId();
  const noteId = `${id}-note`;
  const shown = initial == null ? "" : String(initial);
  const [value, setValue] = useState(shown);
  const last = useRef(shown);
  useEffect(() => {
    setValue(shown);
    last.current = shown;
  }, [shown]);
  const file = useLatest(shown);
  const { save, status, restart } = useSave(table, name, context);
  const commit = (next: string) => {
    if (next === last.current) return;
    last.current = next;
    void save(next).then((ok) => {
      if (ok) onSaved?.(next);
      else if (ok === false) {
        last.current = file.current;
        setValue(file.current);
      }
    });
  };
  return (
    <Row
      id={id}
      label={label}
      title={[...table, name].join(".")}
      feedback={<Feedback status={status} restart={restart} noteId={noteId} />}
    >
      <div
        className="setting-text"
        onBlur={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null))
            commit(value);
        }}
        onKeyDown={(e) => {
          if (
            e.key === "Enter" &&
            (e.target as HTMLElement).tagName === "INPUT"
          )
            commit(value);
        }}
      >
        {models ? (
          <ModelSelect
            id={id}
            value={value}
            choices={models}
            onChange={(next) => {
              setValue(next);
              if (next && models.some((m) => m.value === next)) commit(next);
            }}
          />
        ) : (
          <input
            id={id}
            type="text"
            value={value}
            autoComplete="off"
            spellCheck={false}
            onChange={(e) => setValue(e.target.value)}
          />
        )}
      </div>
    </Row>
  );
}

/** Settings › Agents › New thread defaults: what a new thread starts with, and how long
 * the hub keeps what threads leave behind. Each row saves when it changes. */
export function ThreadDefaults({
  loaded,
  info,
  show,
  context,
}: {
  /** The settings as the config file has them (`desktop.config.read`), else the hub's. */
  loaded: Json;
  /** `hub.info` (the agents it found), if read. */
  info?: Json;
  /** `orchestrator.show` (runtimes, the model catalog), if read. */
  show?: Json;
  context: SaveContext;
}) {
  const [runtime, setRuntime] = useState<string>(loaded?.default_runtime || "");
  const [summaries, setSummaries] = useState(!!loaded?.tui?.summaries);
  useEffect(() => setRuntime(loaded?.default_runtime || ""), [loaded]);
  useEffect(() => setSummaries(!!loaded?.tui?.summaries), [loaded]);
  const runtimes = runtimeChoices(info, show, loaded?.default_runtime);
  // `[threads] model` applies to threads of every runtime: the default runtime's models.
  const models = modelChoices(show?.models, runtime);
  const value = (table: string[], key: string) =>
    settingValue(loaded, table, key);
  const summary = summaryEveryChoices(
    loaded?.tui?.summary_every,
    loaded?.tui?.summary_every_secs,
  );
  const codex = runtimeInstalled("codex", info, show);
  return (
    <section className="settings-section setting-controls">
      <h2>New thread defaults</h2>
      <p>What new threads start with. Changes save as you make them.</p>
      {runtimes.length ? (
        <SettingSelect
          table={[]}
          name="default_runtime"
          label="Default runtime"
          initial={value([], "default_runtime")}
          choices={runtimes.map((r) => ({ value: r, label: r }))}
          context={context}
          onSaved={setRuntime}
        />
      ) : (
        <SettingText
          table={[]}
          name="default_runtime"
          label="Default runtime"
          initial={value([], "default_runtime")}
          context={context}
          onSaved={setRuntime}
        />
      )}
      <SettingSelect
        table={["threads"]}
        name="default_mode"
        label="Default permission mode"
        initial={value(["threads"], "default_mode")}
        choices={permissionChoices(value(["threads"], "default_mode"))}
        context={context}
      />
      <SettingText
        table={["threads"]}
        name="model"
        label="Default model"
        initial={value(["threads"], "model")}
        models={models.length ? models : undefined}
        context={context}
      />
      {document.documentElement.dataset.mobile !== "true" && (
        <ProjectFolders
          roots={loaded?.project_roots}
          defaults={loaded?.default_project_roots}
          context={context}
        />
      )}
      <SettingSwitch
        table={["threads"]}
        name="worktree"
        label="Separate worktrees"
        detail="Each thread works in a worktree of its own. Off, threads work in the project's checkout."
        initial={value(["threads"], "worktree")}
        context={context}
      />
      <SettingSwitch
        table={["threads"]}
        name="thinking"
        label="Include reasoning events"
        initial={value(["threads"], "thinking")}
        context={context}
      />
      <SettingSelect
        table={["threads"]}
        name="idle_compaction"
        label="Automatic context compaction"
        hint="When an idle thread's prompt cache runs out."
        initial={value(["threads"], "idle_compaction")}
        choices={withCurrent(
          COMPACTION_CHOICES,
          value(["threads"], "idle_compaction"),
        )}
        context={context}
      />
      {codex && (
        <SettingSelect
          table={["codex"]}
          name="auto"
          label="Codex commands its sandbox blocks"
          initial={value(["codex"], "auto")}
          choices={withCurrent(CODEX_AUTO_CHOICES, value(["codex"], "auto"))}
          context={context}
        />
      )}
      <SettingSelect
        table={["tui"]}
        name="card_order"
        label="Sort threads"
        initial={value(["tui"], "card_order")}
        choices={withCurrent(CARD_ORDER_CHOICES, value(["tui"], "card_order"))}
        context={context}
      />
      <SettingSwitch
        table={["tui"]}
        name="summaries"
        label="Summarize routine thread updates"
        detail="A small model writes one line per active thread."
        initial={value(["tui"], "summaries")}
        context={context}
        onSaved={setSummaries}
      />
      {summaries && (
        <SettingSelect
          table={["tui"]}
          name="summary_every"
          label="Summary interval"
          hint="At most one summary per thread this often."
          initial={summary.value}
          choices={summary.choices}
          context={context}
        />
      )}
      <SettingSwitch
        table={["tui"]}
        name="suggest"
        label="Suggest next messages"
        detail="A small model suggests what you might send next."
        initial={value(["tui"], "suggest")}
        context={context}
      />
      <SettingSelect
        table={[]}
        name="stall_after_secs"
        label="Stall notification"
        hint="When a running turn makes no progress this long."
        initial={value([], "stall_after_secs")}
        choices={durationChoices(STALL_PRESETS, value([], "stall_after_secs"))}
        number
        context={context}
      />
      <SettingSelect
        table={["trash"]}
        name="keep_days"
        label="Keep deleted threads"
        initial={value(["trash"], "keep_days")}
        choices={durationChoices(
          TRASH_DAYS_PRESETS,
          value(["trash"], "keep_days"),
          "days",
        )}
        number
        confirm={(next) =>
          next !== "0" ||
          window.confirm(
            "Permanently purge deleted threads at the next cleanup? A retention of zero days deletes their worktrees and branches. This cannot be undone.",
          )
        }
        context={context}
      />
      <details className="setting-advanced">
        <summary>Advanced</summary>
        <SettingSelect
          table={["log"]}
          name="retention_days"
          label="Keep full agent logs"
          initial={value(["log"], "retention_days")}
          choices={durationChoices(
            LOG_DAYS_PRESETS,
            value(["log"], "retention_days"),
            "days",
          )}
          number
          context={context}
        />
        <SettingSwitch
          table={["log"]}
          name="archive"
          label="Archive old logs"
          detail="Older logs move to an archive file. Off, they are deleted."
          initial={value(["log"], "archive")}
          context={context}
        />
        <SettingSelect
          table={[]}
          name="background_timeout_secs"
          label="Background timeout"
          hint="Background work that shows no sign of life this long is stopped."
          initial={value([], "background_timeout_secs")}
          choices={durationChoices(
            BACKGROUND_TIMEOUT_PRESETS,
            value([], "background_timeout_secs"),
          )}
          number
          context={context}
        />
        {value([], "wake_after_secs") !== undefined && (
          <SettingSelect
            table={[]}
            name="wake_after_secs"
            label="Wake a thread after its background work"
            hint="How long the hub waits before telling an idle thread its background work ended."
            initial={value([], "wake_after_secs")}
            choices={durationChoices(
              WAKE_AFTER_PRESETS,
              value([], "wake_after_secs"),
            )}
            number
            context={context}
          />
        )}
      </details>
    </section>
  );
}

/** `project_roots`: the folders New thread looks in for a project given by name. Unset is
 * the built-in list (shown as such); the first change writes the whole list. */
function ProjectFolders({
  roots,
  defaults,
  context,
}: {
  roots?: string[] | null;
  defaults?: string[];
  context: SaveContext;
}) {
  const hostApi = useHostAPI();
  const id = useId();
  const [list, setList] = useState<string[] | null>(roots ?? null);
  useEffect(() => setList(roots ?? null), [roots]);
  const file = useLatest(roots);
  // Each change writes the whole list: one at a time, the latest winning, so two quick
  // removals can't land out of order and bring a folder back.
  const { save, status, restart } = useSave([], "project_roots", context);
  const shown = list ?? defaults ?? [];
  const change = (next: string[]) => {
    setList(next);
    void save(next).then((ok) => {
      if (ok === false) setList(file.current ?? null);
    });
  };
  return (
    <div
      className="setting-field project-folders"
      role="group"
      aria-labelledby={`${id}-label`}
      aria-describedby={`${id}-hint`}
      title="project_roots"
    >
      <div className="project-folders-head">
        <span id={`${id}-label`}>Project folders</span>
        <small id={`${id}-hint`}>
          New thread finds a project you name in these folders.
          {list === null && shown.length > 0 && " These are the defaults."}
        </small>
      </div>
      {shown.length ? (
        <ul>
          {shown.map((root) => (
            <li key={root}>
              <code>{root}</code>
              <button
                type="button"
                className="icon-button"
                aria-label={`Remove ${root}`}
                title="Remove"
                onClick={() => change(shown.filter((r) => r !== root))}
              >
                ×
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="muted">None: name projects by their full path.</p>
      )}
      <div className="manager-actions">
        <button
          type="button"
          onClick={() =>
            void hostApi.pickDirectory().then((path) => {
              if (path && !shown.includes(path)) change([...shown, path]);
            })
          }
        >
          Add folder…
        </button>
        {list !== null && (
          <button
            type="button"
            onClick={() => {
              // Back to the built-in list: the closest the one-key writer gets to unsetting.
              if (defaults) change([...defaults]);
            }}
            disabled={!defaults}
          >
            Use defaults
          </button>
        )}
      </div>
      <Feedback status={status} restart={restart} noteId={`${id}-note`} />
    </div>
  );
}
