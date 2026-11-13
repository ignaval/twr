/* Review loops in the apps (docs/design/loops.md › Desktop and mobile): the loop chip on a
 * thread's sidebar card, the Review loop offer, the inspector's Loops section, the setup
 * card, the settings editor and Settings › Loops. Logic lives in loops.ts; App provides the
 * actions through LoopContext, so cards rendered anywhere reach the same ones. */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { AlertCircle, Check, RefreshCw, Repeat } from "lucide-react";
import type { Attention, Json, Thread } from "./types";
import { CopyButton, Dialog, ModelSelect, Spinner } from "./ui";
import { errorMessage } from "./errors";
import { announce, scrollBehavior } from "./a11y";
import { mayAnswer } from "./arming";
import { canRespond } from "./attention";
import { useArming } from "./useArming";
import {
  SETUP_CANCEL,
  SETUP_START,
  editSettings,
  editorRequest,
  hasSubagents,
  matchingPreset,
  modelWords,
  outcomeWords,
  phaseWords,
  phasesOf,
  roundWords,
  skippedPhases,
  startedWords,
  runtimeUsable,
  setupCardState,
  settingsLine,
  waitingWords,
  type EditorChange,
  type EditorSource,
  type EditorState,
  type LoopAccess,
  type LoopControlAction,
  type LoopSetup,
  type LoopState,
  type ReviewRequest,
  type RuntimeInfo,
  type SettingsGet,
} from "./loops";

type Rpc = (method: string, params?: Json) => Promise<Json>;

/** What App gives every loop control. */
export interface LoopActions {
  rpc: Rpc;
  fail: (error: unknown) => void;
  /** Whether this device may act on loops now, and why not. */
  access: LoopAccess;
  /** `[loops] offer_review`. */
  offerReview: boolean;
  /** The model pickers' catalog (`orchestrator.show` `models`), for descriptions. */
  catalog?: Json;
  /** Starts a review loop (`loop.request`); App shows what came of it. */
  startReview: (
    thread: string,
    options?: Omit<ReviewRequest, "thread" | "kind">,
  ) => Promise<boolean>;
  /** Controls a thread's running loop (`loop.control`). */
  control: (thread: string, action: LoopControlAction) => Promise<boolean>;
  /** Opens the settings editor. */
  openEditor: (source: EditorSource) => void;
  /** Asks before something that stops or removes, as App's confirmation does. */
  confirm: (c: {
    title: string;
    detail: string;
    action: string;
    destructive?: boolean;
    run: () => void;
  }) => void;
  /** Bumped when saved settings change, so lines that show them read them again. */
  settingsVersion: number;
  /** Saved settings changed here: lines that show them read them again. */
  settingsChanged: () => void;
}

const noop = async () => false;
export const LoopContext = createContext<LoopActions>({
  rpc: async () => {
    throw new Error("Not connected");
  },
  fail: () => {},
  access: { act: false, why: "Not connected" },
  offerReview: false,
  startReview: noop,
  control: noop,
  openEditor: () => {},
  confirm: () => {},
  settingsVersion: 0,
  settingsChanged: () => {},
});
export const useLoops = () => useContext(LoopContext);

/** The thread's sidebar card's loop chip: the hub's badge and line, nothing else. */
export function LoopChip({ thread }: { thread: Thread }) {
  if (typeof thread.loop_line !== "string" || !thread.loop_line) return null;
  return (
    <span className="loop-chip" title="Show the loop's progress">
      <span className="loop-badge">{thread.loop_badge || "LOOP"}</span>
      <span className="visually-hidden">: </span>
      <span className="loop-line">{thread.loop_line}</span>
    </span>
  );
}

/** The quiet Review loop button on a finished turn with changes. */
export function ReviewLoopOffer({
  thread,
  className = "",
  label = "Review loop",
}: {
  thread: Thread;
  className?: string;
  label?: string;
}) {
  const loops = useLoops();
  const [busy, setBusy] = useState(false);
  return (
    <button
      type="button"
      className={`loop-offer ${className}`}
      disabled={!loops.access.act || busy}
      title={
        loops.access.why ||
        `Start a review loop on ${thread.name}'s changes: a reviewer model checks them round by round and the thread fixes what holds up.`
      }
      aria-label={`${label}: ${thread.name}`}
      onClick={() => {
        setBusy(true);
        void loops.startReview(thread.id).finally(() => setBusy(false));
      }}
    >
      {busy ? <Spinner /> : <Repeat size={13} aria-hidden="true" />}
      {label}
    </button>
  );
}

// ---------------------------------------------------------------------------------------
// The inspector's Loops section
// ---------------------------------------------------------------------------------------

/** `loop.settings.get` for `project`, read again when saved settings change. */
function useLoopSettings(
  project: string | undefined,
  enabled = true,
  refreshKey: unknown = 0,
) {
  const { rpc, settingsVersion } = useLoops();
  const [got, setGot] = useState<SettingsGet>();
  const [error, setError] = useState("");
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    void rpc("loop.settings.get", project ? { project } : {})
      .then((value) => {
        if (!live) return;
        setGot(value);
        setError("");
      })
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [rpc, project, settingsVersion, enabled, tick, refreshKey]);
  return { got, error, reload: () => setTick((n) => n + 1) };
}

export function LoopsSection({
  thread,
  connected,
  focus = false,
  onFocused,
}: {
  thread: Thread;
  connected: boolean;
  /** The thread's loop chip asked for this section: bring it into view, then say so. */
  focus?: boolean;
  onFocused?: () => void;
}) {
  const loops = useLoops();
  const root = useRef<HTMLElement>(null);
  const headingId = useId();
  const state = thread.loop as LoopState | undefined;
  const last = thread.last_loop as Json;
  // Read again when a loop starts or ends: its setup card may have saved settings.
  const settings = useLoopSettings(
    thread.project,
    connected,
    `${state?.id || ""}:${last?.id || ""}`,
  );
  const [detail, setDetail] = useState<Json>();
  const [detailError, setDetailError] = useState("");
  const [busy, setBusy] = useState("");
  const [reportOpen, setReportOpen] = useState(false);
  // `loop.get` again whenever the thread's loop (or its last one) changes.
  const loopKey = JSON.stringify([state ?? null, last ?? null]);
  useEffect(() => {
    if (!connected || (!state && !last)) {
      setDetail(undefined);
      return;
    }
    let live = true;
    void loops
      .rpc("loop.get", { thread: thread.id })
      .then((value) => {
        if (!live) return;
        setDetail(value);
        setDetailError("");
      })
      .catch((e) => live && setDetailError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [loopKey, thread.id, connected, loops.rpc]);
  useEffect(() => {
    if (!focus || !root.current) return;
    root.current.scrollIntoView({ block: "start", behavior: scrollBehavior() });
    root.current.focus({ preventScroll: true });
    onFocused?.();
  }, [focus]);
  const act = loops.access.act;
  const run = (what: string, job: () => Promise<unknown>) => {
    setBusy(what);
    void job().finally(() => setBusy(""));
  };
  const control = (action: LoopControlAction) =>
    run(action, () => loops.control(thread.id, action));
  const reportPath: string | undefined =
    typeof detail?.report_path === "string" ? detail.report_path : undefined;
  const folder: string | undefined =
    typeof detail?.folder === "string" ? detail.folder : undefined;
  return (
    <section
      className="inspector-loops"
      aria-labelledby={headingId}
      ref={root}
      tabIndex={-1}
      id={`twr-loops-${thread.id}`}
    >
      <h3 id={headingId}>Loops</h3>
      {state ? (
        <LoopProgress
          state={state}
          thread={thread}
          detail={detail?.id === state.id ? detail : undefined}
        />
      ) : (
        <>
          <button
            type="button"
            className="full-width"
            disabled={!act || !!busy}
            title={loops.access.why}
            onClick={() => run("review", () => loops.startReview(thread.id))}
          >
            {busy === "review" ? (
              <Spinner />
            ) : (
              <Repeat size={15} aria-hidden="true" />
            )}
            Review loop
          </button>
          <p className="loop-settings-line">
            {settings.got ? (
              <span>{settingsLine(settings.got)}</span>
            ) : settings.error ? (
              <span className="error-text">{settings.error}</span>
            ) : connected ? (
              <Spinner />
            ) : (
              <span className="muted">Connect to see the loop settings.</span>
            )}{" "}
            <button
              type="button"
              className="text-button"
              disabled={!connected}
              aria-haspopup="dialog"
              onClick={() =>
                loops.openEditor({ kind: "saved", project: thread.project })
              }
            >
              Change…
            </button>
          </p>
          {last && (
            <p className="muted loop-last">
              Last {last.kind === "implement" ? "implementation" : "review"}{" "}
              loop: {outcomeWords(last.outcome)}
            </p>
          )}
        </>
      )}
      {detailError && <p className="error-text">{detailError}</p>}
      {state && (
        <div className="loop-controls" role="group" aria-label="Loop controls">
          {state.paused ? (
            <button
              type="button"
              disabled={!act || !!busy}
              title={loops.access.why}
              onClick={() => control("resume")}
            >
              {busy === "resume" && <Spinner />}
              Resume
            </button>
          ) : (
            <button
              type="button"
              disabled={!act || !!busy}
              title={loops.access.why}
              onClick={() => control("pause")}
            >
              {busy === "pause" && <Spinner />}
              Pause
            </button>
          )}
          {["scope", "ladder"].includes(state.phase) && !state.skip && (
            <button
              type="button"
              disabled={!act || !!busy}
              title={
                loops.access.why ||
                "End the ladder after the current round and go on to the capped verification"
              }
              onClick={() => control("skip_to_verify")}
            >
              {busy === "skip_to_verify" && <Spinner />}
              Skip to verify
            </button>
          )}
          <button
            type="button"
            className="danger-quiet"
            disabled={!act || !!busy}
            title={loops.access.why}
            onClick={() =>
              loops.confirm({
                title: `Stop ${thread.name}'s review loop?`,
                detail:
                  "The loop ends as stopped. The thread and its work stay as they are.",
                action: "Stop loop",
                destructive: true,
                run: () => control("stop"),
              })
            }
          >
            {busy === "stop" && <Spinner />}
            Stop…
          </button>
        </div>
      )}
      {(reportPath || folder) && (
        <div className="loop-report">
          {reportPath && (
            <>
              <button
                type="button"
                aria-expanded={reportOpen}
                onClick={() => setReportOpen((v) => !v)}
              >
                Open report
              </button>
              {reportOpen && (
                <div className="loop-path">
                  <code>{reportPath}</code>
                  <CopyButton
                    label="Copy report path"
                    text={() => reportPath}
                    onError={loops.fail}
                  />
                </div>
              )}
            </>
          )}
          {folder && (
            <details>
              <summary>Loop folder</summary>
              <div className="loop-path">
                <code>{folder}</code>
                <CopyButton
                  label="Copy loop folder path"
                  text={() => folder}
                  onError={loops.fail}
                />
              </div>
            </details>
          )}
        </div>
      )}
    </section>
  );
}

const severityOrder: Record<string, number> = {
  blocker: 0,
  major: 1,
  minor: 2,
};

function LoopProgress({
  state,
  detail,
  thread,
}: {
  state: LoopState;
  detail?: Json;
  thread: Thread;
}) {
  const phases = phasesOf(state.kind);
  const started = startedWords(detail?.started, thread);
  const current = phases.indexOf(state.phase);
  const skipped = skippedPhases(state);
  const rounds: Json[] = Array.isArray(detail?.rounds) ? detail.rounds : [];
  return (
    <div className="loop-progress">
      <p className="loop-status" role="status">
        <span className="loop-badge">
          {state.kind === "implement" ? "IMPLEMENT LOOP" : "REVIEW LOOP"}
        </span>{" "}
        {state.profile} · {waitingWords(state)}
      </p>
      <ol className="loop-phases" aria-label="Phases">
        {phases.map((phase, i) => {
          const passed = i < current && skipped.includes(phase);
          const done = i < current && !passed;
          return (
            <li
              key={phase}
              aria-current={i === current ? "step" : undefined}
              className={
                i === current
                  ? "current"
                  : passed
                    ? "skipped"
                    : done
                      ? "done"
                      : undefined
              }
            >
              {done && (
                <Check size={12} aria-hidden="true" className="loop-done" />
              )}
              {phaseWords(phase)}
              {done && <span className="visually-hidden"> (done)</span>}
              {passed && " skipped"}
            </li>
          );
        })}
      </ol>
      <dl className="loop-facts">
        {started && (
          <>
            <dt>Runs on</dt>
            <dd>{started}</dd>
          </>
        )}
        <dt>Reviewer</dt>
        <dd>
          {modelWords({
            runtime: state.reviewer.runtime,
            model: state.reviewer.model,
            effort: state.reviewer.effort || undefined,
          })}
        </dd>
        <dt>Ledger</dt>
        <dd>
          {state.ledger} {state.ledger === 1 ? "entry" : "entries"}
        </dd>
        {state.verify?.cap > 0 && (
          <>
            <dt>Verify</dt>
            <dd>
              {state.verify.done} of {state.verify.cap} rounds done
            </dd>
          </>
        )}
      </dl>
      {state.rounds.length > 0 && (
        <ul className="loop-rounds" aria-label="Rounds">
          {state.rounds.map((round, index) => {
            const full = rounds.find((r) => r.id === round.id);
            const findings: Json[] = Array.isArray(full?.findings)
              ? [...full.findings].sort(
                  (a, b) =>
                    (severityOrder[a.severity] ?? 3) -
                    (severityOrder[b.severity] ?? 3),
                )
              : [];
            const triage: Json[] = Array.isArray(full?.triage?.items)
              ? full.triage.items
              : [];
            const words = roundWords(round, state);
            return (
              <li key={round.id}>
                {findings.length || triage.length || full?.reason ? (
                  <details>
                    <summary>
                      <b>{round.id}</b> {words}
                    </summary>
                    {full?.reason && (
                      <p className="error-text">{full.reason}</p>
                    )}
                    {findings.map((f, i) => {
                      const verdict = triage.find((t) => t.id === f.id);
                      return (
                        <div className="loop-finding" key={f.id || i}>
                          <p>
                            <b>{f.id}</b>{" "}
                            <span className={`loop-severity ${f.severity}`}>
                              {f.severity}
                            </span>{" "}
                            <code>{f.location}</code>
                          </p>
                          <p>{f.problem}</p>
                          {verdict && (
                            <p className="loop-verdict">
                              {verdict.verdict}
                              {verdict.class ? ` · ${verdict.class}` : ""}
                              {verdict.rule ? ` · ${verdict.rule}` : ""}
                              {verdict.of ? ` · of ${verdict.of}` : ""}
                              {verdict.reason ? `: ${verdict.reason}` : ""}
                            </p>
                          )}
                        </div>
                      );
                    })}
                    {full?.triage?.gates && (
                      <p className="muted">
                        Gates {full.triage.gates.result}
                        {full.triage.gates.ran?.length
                          ? `: ${full.triage.gates.ran.join(", ")}`
                          : ""}
                      </p>
                    )}
                  </details>
                ) : (
                  <span>
                    <b>{round.id}</b> {words}
                  </span>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------------------
// The setup card
// ---------------------------------------------------------------------------------------

/** Setup cards whose sheet is open, by attention id. */
const openSheets = new Set<string>();

const isPhone = () =>
  typeof document !== "undefined" &&
  document.documentElement.dataset.mobile === "true" &&
  typeof window.matchMedia === "function" &&
  window.matchMedia("(max-width: 799px)").matches;

/**
 * A loop's setup card: what the loop would run on, as the hub wrote it, and Start and save,
 * Change… and Cancel. Start and Cancel answer the revision shown, under the 600 ms rule of
 * every request card; Start waits while the card lists something to change first. On a
 * phone the card is a row that opens as a sheet.
 */
export function LoopSetupCard({
  item,
  thread,
  readOnly,
  onRespond,
}: {
  item: Attention;
  thread?: Thread;
  readOnly: boolean;
  onRespond: (item: Attention, answer: string) => Promise<void>;
}) {
  // The hub raises the card again at a new revision when its editor saves, and lists key
  // a card by revision: an open sheet stays open across that remount.
  const [sheet, setSheetState] = useState(() => openSheets.has(item.attn_id));
  const opener = useRef<HTMLButtonElement>(null);
  const setSheet = (open: boolean) => {
    if (open) openSheets.add(item.attn_id);
    else openSheets.delete(item.attn_id);
    setSheetState(open);
  };
  const { parsed } = setupCardState(item);
  const phone = isPhone();
  const card = (
    <SetupCardBody
      item={item}
      thread={thread}
      readOnly={readOnly}
      onRespond={onRespond}
    />
  );
  if (!phone && !sheet) return card;
  const dialog = sheet && (
    <Dialog
      title={`Loop setup · ${thread?.name || "twr"}`}
      onClose={() => {
        setSheet(false);
        // The sheet that opened before a new revision remembers no opener of its own.
        requestAnimationFrame(() => opener.current?.focus());
      }}
    >
      {card}
    </Dialog>
  );
  // A sheet opened on a phone that has since grown wider (turned sideways) stays a sheet.
  if (!phone) return dialog;
  // The row stays while its sheet is open, so closing the sheet gives focus back to it.
  return (
    <>
      <article
        className="attention-card attention-loop-setup loop-setup-row"
        aria-label={`Loop setup for ${thread?.name || "a thread"}`}
      >
        <header>
          <span className="attention-kind">
            <Repeat size={14} aria-hidden="true" />
            Loop setup
          </span>
          <span>{thread?.name || "twr"}</span>
        </header>
        <div className="attention-body">
          <p>{parsed.title || item.summary.split("\n")[0]}</p>
          <button
            type="button"
            className="primary"
            ref={opener}
            aria-haspopup="dialog"
            aria-expanded={sheet}
            onClick={() => setSheet(true)}
          >
            Review setup
          </button>
        </div>
      </article>
      {dialog}
    </>
  );
}

function SetupCardBody({
  item,
  thread,
  readOnly,
  onRespond,
}: {
  item: Attention;
  thread?: Thread;
  readOnly: boolean;
  onRespond: (item: Attention, answer: string) => Promise<void>;
}) {
  const loops = useLoops();
  const ids = useId();
  const response = useRef<HTMLDivElement>(null);
  const arming = useArming(response, item.attn_id + ":" + item.revision);
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState(false);
  const [error, setError] = useState("");
  const pending = useRef(false);
  const { parsed, canStart, canCancel } = setupCardState(item);
  const settle = async (answer: string) => {
    if (
      pending.current ||
      sent ||
      !canRespond(item, busy) ||
      !mayAnswer({
        arming: arming.state(),
        now: performance.now(),
        busy,
        sent,
        readOnly,
      })
    )
      return;
    pending.current = true;
    setBusy(true);
    setError("");
    try {
      await onRespond(item, answer);
      setSent(true);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      pending.current = false;
      setBusy(false);
    }
  };
  const locked = readOnly || busy || sent || !item.revision || item.incomplete;
  const waiting = !locked && !arming.armed;
  const fallback = !parsed.rows.length;
  return (
    <article
      className="attention-card attention-loop-setup"
      aria-labelledby={`${ids}-kind ${ids}-thread`}
      aria-describedby={`${ids}-title`}
      data-thread={item.thread}
      tabIndex={-1}
    >
      <header>
        <span className="attention-kind" id={`${ids}-kind`}>
          <Repeat size={14} aria-hidden="true" />
          Loop setup
        </span>
        <span id={`${ids}-thread`}>{thread?.name || "twr"}</span>
      </header>
      <div className="attention-body">
        {fallback ? (
          <pre className="attention-summary" id={`${ids}-title`}>
            {item.summary}
          </pre>
        ) : (
          <>
            <p className="loop-setup-title" id={`${ids}-title`}>
              {parsed.title}
            </p>
            <dl className="loop-setup-rows">
              {parsed.rows.map(([label, value], i) => (
                <div key={label + i}>
                  <dt>{label}</dt>
                  <dd>{value}</dd>
                </div>
              ))}
            </dl>
            {parsed.other.map((line, i) => (
              <p key={i}>{line}</p>
            ))}
          </>
        )}
        {(parsed.warnings.length > 0 || parsed.problems.length > 0) && (
          <ul className="loop-notes">
            {parsed.warnings.map((w, i) => (
              <li className="loop-warning" key={"w" + i}>
                <AlertCircle size={13} aria-hidden="true" />
                <span>
                  <span className="visually-hidden">Warning: </span>
                  {w}
                </span>
              </li>
            ))}
            {parsed.problems.map((p, i) => (
              <li className="loop-problem" key={"p" + i}>
                <AlertCircle size={13} aria-hidden="true" />
                <span>
                  <b>Change first:</b> {p}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
      {readOnly && (
        <p className="muted loop-readonly">
          {loops.access.why || "This device has read-only access."}
        </p>
      )}
      <div className="attention-response" ref={response}>
        <div
          className={`attention-options ${waiting ? "arming" : ""}`}
          data-armed={arming.armed && !locked ? "true" : "false"}
        >
          <button
            type="button"
            className="primary"
            disabled={locked || !canStart}
            aria-disabled={waiting || undefined}
            title={
              parsed.problems.length
                ? "Change what the card lists first"
                : undefined
            }
            onClick={() => void settle(SETUP_START)}
          >
            Start and save
          </button>
          <button
            type="button"
            disabled={readOnly || busy || sent || !loops.access.act}
            aria-haspopup="dialog"
            onClick={() =>
              loops.openEditor({
                kind: "card",
                attnId: item.attn_id,
                thread: item.thread,
                project: thread?.project,
              })
            }
          >
            Change…
          </button>
          <button
            type="button"
            disabled={locked || !canCancel}
            aria-disabled={waiting || undefined}
            onClick={() => void settle(SETUP_CANCEL)}
          >
            Cancel
          </button>
          {busy && <Spinner />}
          {sent && (
            <span className="attention-sent" role="status">
              <Check size={14} aria-hidden="true" />
              Sent
            </span>
          )}
        </div>
        {error && (
          <p role="alert" className="error-text">
            {error} Review the current card before trying again.
          </p>
        )}
      </div>
    </article>
  );
}

// ---------------------------------------------------------------------------------------
// The settings editor
// ---------------------------------------------------------------------------------------

/** The editor in a dialog: a setup card's draft, or saved settings. */
export function LoopEditorDialog({
  source,
  thread,
  onClose,
}: {
  source: EditorSource;
  thread?: Thread;
  onClose: () => void;
}) {
  return (
    <Dialog
      title={
        source.kind === "card"
          ? `Loop settings · ${thread?.name || "setup card"}`
          : source.project
            ? `Loop settings · ${source.project}`
            : "Loop settings"
      }
      subtitle={
        source.kind === "card"
          ? "Change what this setup card shows. You go back to the card to start the loop."
          : "The settings loops start on, without a setup card."
      }
      onClose={onClose}
      wide
    >
      <LoopSettingsForm
        source={source}
        thread={thread}
        onDone={source.kind === "card" ? onClose : undefined}
        onCancel={onClose}
      />
    </Dialog>
  );
}

/**
 * The settings editor: the presets, the implementer, its workers, the reviewer, the profile,
 * what a review loop commits and where it saves. One component for a setup card's draft
 * (`loop.setup.get` / `loop.setup.update`) and for saved settings (`loop.settings.get` /
 * `loop.settings.set`), for the workspace or a project. The hub's warnings and problems show
 * as it returns them.
 */
export function LoopSettingsForm({
  source,
  thread,
  onDone,
  onCancel,
  inline = false,
}: {
  source: EditorSource;
  thread?: Thread;
  /** After a save (a card's editor goes back to the card). Saved settings are read again,
   * with the hub's warnings and problems. */
  onDone?: () => void;
  onCancel?: () => void;
  inline?: boolean;
}) {
  const loops = useLoops();
  const ids = useId();
  const project = source.project || thread?.project;
  const [got, setGot] = useState<SettingsGet>();
  const [state, setState] = useState<EditorState>();
  const [card, setCard] = useState<LoopSetup>();
  const [loadError, setLoadError] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [tick, setTick] = useState(0);
  const readOnly = !loops.access.act;
  const load = useCallback(async () => {
    setLoadError("");
    const settings: SettingsGet = await loops.rpc(
      "loop.settings.get",
      project ? { project } : {},
    );
    if (source.kind === "card") {
      const shown = await loops.rpc("loop.setup.get", {
        attn_id: source.attnId,
      });
      const setup: LoopSetup = shown.setup;
      setGot(settings);
      setCard(setup);
      setState({
        settings: setup.settings,
        saveFor: setup.save_for || "all",
        revision: shown.revision,
        project: setup.project || project,
      });
      return shown.revision as number;
    }
    setGot(settings);
    setState({
      settings: settings.settings || settings.first_card,
      saveFor:
        source.project && settings.source === "project" ? "project" : "all",
      project,
    });
    return undefined;
  }, [
    loops.rpc,
    source.kind,
    source.kind === "card" ? source.attnId : "",
    project,
  ]);
  useEffect(() => {
    let live = true;
    void load().catch((e) => live && setLoadError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [load, tick, loops.settingsVersion]);
  const runtimes: RuntimeInfo[] = got?.runtimes || [];
  const change = (c: EditorChange) => {
    setNotice("");
    setState((old) => (old ? editSettings(old, c, runtimes) : old));
  };
  const save = async () => {
    if (!state || readOnly) return;
    const s = state.settings;
    if (s.implementer && !s.implementer.model?.trim())
      return setError("Choose the implementer's model.");
    if (!s.reviewer.model?.trim())
      return setError("Choose the reviewer's model.");
    setBusy(true);
    setError("");
    setNotice("");
    const shownRevision = state.revision;
    try {
      const request = editorRequest(source, state);
      await loops.rpc(request.method, request.params);
      if (source.kind === "card") {
        announce("Setup card updated");
        onDone?.();
      } else {
        setNotice("Saved. Loops start on these settings from now on.");
        announce("Loop settings saved");
        loops.settingsChanged();
        onDone?.();
      }
    } catch (e) {
      const message = errorMessage(e);
      if (source.kind === "card") {
        // The card may have been raised again (or closed) since it was shown: read it
        // again, and say so when it changed.
        try {
          const revision = await load();
          setError(
            revision !== shownRevision
              ? "The setup card changed since you opened it. It shows its current settings now: check them and save again."
              : message,
          );
        } catch (reload) {
          setError(`${message} ${errorMessage(reload)}`);
        }
      } else setError(message);
    } finally {
      setBusy(false);
    }
  };
  if (loadError)
    return (
      <div className="loop-editor">
        <p className="error-text" role="alert">
          {loadError}
        </p>
        <div className="manager-actions">
          <button type="button" onClick={() => setTick((n) => n + 1)}>
            <RefreshCw size={14} aria-hidden="true" />
            Try again
          </button>
          {onCancel && (
            <button type="button" onClick={onCancel}>
              Close
            </button>
          )}
        </div>
      </div>
    );
  if (!got || !state)
    return (
      <div className="loop-editor transcript-loading">
        <Spinner />
        Loading loop settings…
      </div>
    );
  const s = state.settings;
  const runtimeOf = (name?: string) => runtimes.find((r) => r.runtime === name);
  const describe = (runtime: string | undefined, values: string[]) =>
    values.map((value) => ({
      value,
      description: loops.catalog?.[runtime || ""]?.models?.find(
        (m: Json) => m?.id === value,
      )?.description,
    }));
  // An effort the runtime does not list (or a runtime not known) still shows as it is,
  // never as "Runtime default".
  const efforts = (runtime: string | undefined, current?: string) => {
    const listed = runtimeOf(runtime)?.efforts || [];
    return [
      ...(current && !listed.includes(current) ? [current] : []),
      ...listed,
    ].map((value) => ({ value }));
  };
  const implementerRuntime = s.implementer?.runtime || thread?.runtime;
  const showWorkers =
    !s.implementer && !thread
      ? true
      : hasSubagents(implementerRuntime, runtimes);
  // Saved settings with the thread's own implementer name no runtime: the workers' choices
  // are those of a runtime that has subagents.
  const workersRuntime =
    implementerRuntime ||
    runtimes.find((r) => hasSubagents(r.runtime, runtimes))?.runtime;
  const reviewerAccounts = runtimeOf(s.reviewer.runtime)?.accounts || [];
  const preset = matchingPreset(s, got.presets || []);
  const warnings = card ? card.warnings || [] : got.warnings || [];
  const problems = card ? card.problems || [] : got.problems || [];
  const runtimeOptions = (selected?: string) =>
    runtimes.map((r) => {
      const usable = runtimeUsable(r);
      return (
        <option
          key={r.runtime}
          value={r.runtime}
          disabled={!usable.usable && r.runtime !== selected}
        >
          {r.runtime}
          {usable.usable
            ? usable.note
              ? ` (${usable.note})`
              : ""
            : ` (${usable.why})`}
        </option>
      );
    });
  const body = (
    <fieldset className="loop-editor" disabled={readOnly || busy}>
      <legend className="visually-hidden">Loop settings</legend>
      {readOnly && (
        <p className="muted">
          {loops.access.why || "This device has read-only access."} The settings
          show as they are.
        </p>
      )}
      {(got.presets || []).length > 0 && (
        <div className="field">
          <span id={`${ids}-presets`}>Preset</span>
          <div
            className="segmented loop-presets"
            role="group"
            aria-labelledby={`${ids}-presets`}
          >
            {got.presets.map((p) => {
              const unusable = [p.implementer.runtime, p.reviewer.runtime]
                .map((rt) => [rt, runtimeUsable(runtimeOf(rt))] as const)
                .find(([, u]) => !u.usable);
              return (
                <button
                  type="button"
                  key={p.id}
                  className={preset === p.id ? "active" : ""}
                  aria-pressed={preset === p.id}
                  title={
                    unusable
                      ? `${unusable[0]} is ${unusable[1].why}`
                      : `${modelWords(p.implementer)}, reviewed by ${modelWords(p.reviewer)}`
                  }
                  onClick={() => change({ kind: "preset", preset: p })}
                >
                  {p.label}
                </button>
              );
            })}
          </div>
        </div>
      )}
      <div
        className="loop-editor-group"
        role="group"
        aria-labelledby={`${ids}-impl-heading`}
      >
        <label className="field" htmlFor={`${ids}-impl`}>
          <span id={`${ids}-impl-heading`}>Implementer</span>
          <select
            id={`${ids}-impl`}
            aria-describedby={`${ids}-impl-hint`}
            value={s.implementer ? s.implementer.runtime : ""}
            onChange={(e) =>
              e.target.value
                ? change({ kind: "implementer", runtime: e.target.value })
                : change({ kind: "implementer", own: true })
            }
          >
            <option value="">
              The thread's own
              {thread
                ? ` (${modelWords({ runtime: thread.runtime, model: thread.model_actual || thread.model, effort: thread.effort })})`
                : ""}
            </option>
            {runtimeOptions(s.implementer?.runtime)}
          </select>
        </label>
        <small id={`${ids}-impl-hint`}>
          A named implementer runs on the thread's own runtime: a thread on
          another runtime gets the setup card.
        </small>
        {s.implementer && (
          <div className="loop-editor-row">
            <label className="field" htmlFor={`${ids}-impl-model`}>
              <span>Model</span>
              <ModelSelect
                id={`${ids}-impl-model`}
                value={s.implementer.model || ""}
                onChange={(model) => change({ kind: "implementer", model })}
                choices={describe(
                  s.implementer.runtime,
                  runtimeOf(s.implementer.runtime)?.models || [],
                )}
                required
              />
            </label>
            <label className="field" htmlFor={`${ids}-impl-effort`}>
              <span>Effort</span>
              <ModelSelect
                id={`${ids}-impl-effort`}
                value={s.implementer.effort || ""}
                onChange={(effort) => change({ kind: "implementer", effort })}
                choices={efforts(s.implementer.runtime, s.implementer.effort)}
                custom={false}
              />
            </label>
          </div>
        )}
      </div>
      {showWorkers && (
        <div
          className="loop-editor-group"
          role="group"
          aria-labelledby={`${ids}-workers-heading`}
          aria-describedby={`${ids}-workers-hint`}
        >
          <span className="loop-editor-heading" id={`${ids}-workers-heading`}>
            Workers
          </span>
          <div className="loop-editor-row">
            <label className="field" htmlFor={`${ids}-workers-model`}>
              <span>Model</span>
              <ModelSelect
                id={`${ids}-workers-model`}
                value={s.workers.model || ""}
                onChange={(model) => change({ kind: "workers", model })}
                choices={describe(
                  workersRuntime,
                  runtimeOf(workersRuntime)?.models || [],
                )}
              />
            </label>
            <label className="field" htmlFor={`${ids}-workers-effort`}>
              <span>Effort</span>
              <ModelSelect
                id={`${ids}-workers-effort`}
                value={s.workers.effort || ""}
                onChange={(effort) => change({ kind: "workers", effort })}
                choices={efforts(workersRuntime, s.workers.effort)}
                custom={false}
              />
            </label>
          </div>
          <small className="muted" id={`${ids}-workers-hint`}>
            The implementer's subagents, where its runtime has them.
          </small>
        </div>
      )}
      <div
        className="loop-editor-group"
        role="group"
        aria-labelledby={`${ids}-rev-heading`}
        aria-describedby={`${ids}-rev-hint`}
      >
        <span className="loop-editor-heading" id={`${ids}-rev-heading`}>
          Reviewer
        </span>
        <div className="loop-editor-row">
          <label className="field" htmlFor={`${ids}-rev`}>
            <span>Runtime</span>
            <select
              id={`${ids}-rev`}
              value={s.reviewer.runtime}
              onChange={(e) =>
                change({ kind: "reviewer", runtime: e.target.value })
              }
            >
              {!runtimeOf(s.reviewer.runtime) && (
                <option value={s.reviewer.runtime}>{s.reviewer.runtime}</option>
              )}
              {runtimeOptions(s.reviewer.runtime)}
            </select>
          </label>
          <label className="field" htmlFor={`${ids}-rev-model`}>
            <span>Model</span>
            <ModelSelect
              id={`${ids}-rev-model`}
              value={s.reviewer.model || ""}
              onChange={(model) => change({ kind: "reviewer", model })}
              choices={describe(
                s.reviewer.runtime,
                runtimeOf(s.reviewer.runtime)?.models || [],
              )}
              required
            />
          </label>
          {reviewerAccounts.length > 1 && (
            <label className="field" htmlFor={`${ids}-rev-account`}>
              <span>Account</span>
              <select
                id={`${ids}-rev-account`}
                value={s.reviewer.account || ""}
                onChange={(e) =>
                  change({ kind: "reviewer", account: e.target.value })
                }
              >
                <option value="">The runtime's default account</option>
                {reviewerAccounts.map((a) => (
                  <option key={a}>{a}</option>
                ))}
              </select>
            </label>
          )}
        </div>
        <small className="muted" id={`${ids}-rev-hint`}>
          Its effort is each round's, from the profile.
        </small>
      </div>
      <div className="loop-editor-row">
        <label className="field" htmlFor={`${ids}-profile`}>
          <span>Profile</span>
          <select
            id={`${ids}-profile`}
            value={s.profile}
            onChange={(e) =>
              change({ kind: "profile", profile: e.target.value })
            }
          >
            {!(got.profiles || []).includes(s.profile) && (
              <option value={s.profile}>{s.profile}</option>
            )}
            {(got.profiles || []).map((p) => (
              <option key={p}>{p}</option>
            ))}
          </select>
        </label>
        <label className="field" htmlFor={`${ids}-plan-check`}>
          <span>Plan check</span>
          <select
            id={`${ids}-plan-check`}
            value={s.plan_check || ""}
            onChange={(e) =>
              change({ kind: "plan_check", value: e.target.value })
            }
          >
            <option value="">The profile's</option>
            <option value="never">Never</option>
            <option value="contracts">When the plan touches contracts</option>
            <option value="always">Always</option>
          </select>
        </label>
        <label className="field" htmlFor={`${ids}-impl-commit`}>
          <span>Implementation commits</span>
          <select
            id={`${ids}-impl-commit`}
            value={s.commit || "end"}
            onChange={(e) => change({ kind: "commit", value: e.target.value })}
          >
            <option value="end">One commit at the end</option>
            <option value="per-round">A commit per round</option>
            <option value="none">No commit</option>
          </select>
        </label>
        <label className="field" htmlFor={`${ids}-commit`}>
          <span>Review commits</span>
          <select
            id={`${ids}-commit`}
            value={s.review_commit || "match"}
            onChange={(e) =>
              change({ kind: "review_commit", value: e.target.value })
            }
          >
            <option value="match">Like the thread (commit if it was)</option>
            <option value="none">Leave fixes uncommitted</option>
          </select>
        </label>
      </div>
      {(source.kind === "card" || source.project) && (
        <div className="field">
          <span id={`${ids}-save-for`}>Save for</span>
          <div
            className="segmented"
            role="group"
            aria-labelledby={`${ids}-save-for`}
          >
            <button
              type="button"
              className={state.saveFor === "all" ? "active" : ""}
              aria-pressed={state.saveFor === "all"}
              onClick={() => change({ kind: "save_for", value: "all" })}
            >
              All projects
            </button>
            <button
              type="button"
              className={state.saveFor === "project" ? "active" : ""}
              aria-pressed={state.saveFor === "project"}
              disabled={!state.project}
              title={
                state.project
                  ? `Only ${state.project}`
                  : "This thread has no project"
              }
              onClick={() => change({ kind: "save_for", value: "project" })}
            >
              {state.project ? `${state.project} only` : "This project"}
            </button>
          </div>
        </div>
      )}
      {(warnings.length > 0 || problems.length > 0) && (
        <ul className="loop-notes" aria-label="What the hub says">
          {warnings.map((w, i) => (
            <li className="loop-warning" key={"w" + i}>
              <AlertCircle size={13} aria-hidden="true" />
              <span>
                <span className="visually-hidden">Warning: </span>
                {w}
              </span>
            </li>
          ))}
          {problems.map((p, i) => (
            <li className="loop-problem" key={"p" + i}>
              <AlertCircle size={13} aria-hidden="true" />
              <span>
                <b>Change first:</b> {p}
              </span>
            </li>
          ))}
        </ul>
      )}
    </fieldset>
  );
  const footer = (
    <>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <span className="success-text" role="status">
        {notice}
      </span>
    </>
  );
  const actions = !readOnly && (
    <button
      type="button"
      className="primary"
      disabled={busy}
      onClick={() => void save()}
    >
      {busy && <Spinner />}
      {source.kind === "card" ? "Save to the card" : "Save settings"}
    </button>
  );
  if (inline)
    return (
      <div className="loop-editor-inline">
        {body}
        {footer}
        <div className="manager-actions">{actions}</div>
      </div>
    );
  return (
    <>
      <div className="operation-fields">
        {body}
        {footer}
      </div>
      <footer className="dialog-footer">
        <span>
          {source.kind === "card"
            ? "The card shows the new values; Start and save starts the loop."
            : state.saveFor === "project" && state.project
              ? `Saves for ${state.project}`
              : "Saves for every project without its own"}
        </span>
        <div>
          <button type="button" onClick={onCancel} disabled={busy}>
            {readOnly || notice ? "Close" : "Cancel"}
          </button>
          {actions}
        </div>
      </footer>
    </>
  );
}

// ---------------------------------------------------------------------------------------
// Settings › Loops
// ---------------------------------------------------------------------------------------

/** Settings › Loops: the saved settings, the projects with their own, and Reset all. */
export function LoopSettingsPanel({ connected }: { connected: boolean }) {
  const loops = useLoops();
  const headingId = useId();
  const [project, setProject] = useState<string>();
  const workspace = useLoopSettings(undefined, connected);
  const [busy, setBusy] = useState("");
  const [asking, setAsking] = useState<{
    title: string;
    detail: string;
    action: string;
    run: () => Promise<void>;
  }>();
  const projects = workspace.got?.projects || [];
  const source = useMemo<EditorSource>(
    () => ({ kind: "saved", project }),
    [project],
  );
  const reset = (params: Json, what: string) => async () => {
    setBusy(what);
    try {
      await loops.rpc("loop.settings.reset", params);
      announce("Loop settings reset");
      if (params.project && params.project === project) setProject(undefined);
      loops.settingsChanged();
    } catch (e) {
      loops.fail(e);
    } finally {
      setBusy("");
    }
  };
  let content: ReactNode;
  if (!connected)
    content = <p className="muted">Connect to the hub to see loop settings.</p>;
  else if (workspace.error)
    content = <p className="error-text">{workspace.error}</p>;
  else
    content = (
      <>
        <p>
          {workspace.got && !workspace.got.settings
            ? "Nothing is saved yet: the first loop shows a setup card that asks you to choose models. You can choose them here instead."
            : "The settings review loops start on. A project with settings of its own uses those instead."}
        </p>
        {projects.length > 0 && (
          <div className="loop-projects">
            <h3 id={`${headingId}-projects`}>
              Projects with their own settings
            </h3>
            <ul aria-labelledby={`${headingId}-projects`}>
              {projects.map((p) => (
                <li key={p}>
                  <span>{p}</span>
                  <button
                    type="button"
                    aria-pressed={project === p}
                    onClick={() => setProject(project === p ? undefined : p)}
                  >
                    {project === p ? "Back to all projects" : "Open"}
                  </button>
                  <button
                    type="button"
                    className="danger-quiet"
                    disabled={!loops.access.act || !!busy}
                    title={loops.access.why}
                    onClick={() =>
                      setAsking({
                        title: `Remove ${p}'s loop settings?`,
                        detail: `${p} uses the settings of all projects again.`,
                        action: "Remove",
                        run: reset({ project: p }, p),
                      })
                    }
                  >
                    {busy === p && <Spinner />}
                    Remove…
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
        <h3 className="loop-settings-for">
          {project ? `${project}'s settings` : "All projects"}
        </h3>
        <LoopSettingsForm key={project || ""} source={source} inline />
        <div className="manager-actions">
          <button
            type="button"
            className="danger-quiet"
            disabled={
              !loops.access.act ||
              !!busy ||
              (!workspace.got?.settings && !projects.length)
            }
            title={loops.access.why}
            onClick={() =>
              setAsking({
                title: "Reset all loop settings?",
                detail:
                  "Every saved set goes, the projects' own too. The next loop shows the setup card again.",
                action: "Reset all",
                run: reset({}, "all"),
              })
            }
          >
            {busy === "all" && <Spinner />}
            Reset all…
          </button>
        </div>
      </>
    );
  return (
    <section className="settings-section" aria-labelledby={headingId}>
      <h2 id={headingId}>Loops</h2>
      {content}
      {asking && (
        <Dialog
          title={asking.title}
          subtitle={asking.detail}
          onClose={() => setAsking(undefined)}
        >
          <footer className="dialog-footer">
            <span />
            <div>
              <button type="button" onClick={() => setAsking(undefined)}>
                Cancel
              </button>
              <button
                type="button"
                className="danger"
                onClick={() => {
                  const run = asking.run;
                  setAsking(undefined);
                  void run();
                }}
              >
                {asking.action}
              </button>
            </div>
          </footer>
        </Dialog>
      )}
    </section>
  );
}
