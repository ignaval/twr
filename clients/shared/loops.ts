/* Loops in the apps (docs/design/loops.md › Desktop and mobile): the pure logic behind the
 * inspector's Loops section, the loop chip, the Review loop offer, Send as implementation
 * loop, the New thread form's switch, the setup card and the settings editor. Nothing here decides what the hub decides: who may start a loop,
 * on which settings, whether settings hold, the loop line. The hub says; this reads it. */
import type { Attention, Json, Thread } from "./types";

/** `runtime[:account]/model[:effort]`: a named implementer (twr-core `ModelChoice`). */
export interface ModelChoice {
  runtime: string;
  account?: string;
  model: string;
  effort?: string;
}
/** The reviewer: its effort is each round's, from the profile (twr-core `ReviewerChoice`). */
export interface ReviewerChoice {
  runtime: string;
  account?: string;
  model: string;
}
/** The implementer's subagents, on its runtime (twr-core `WorkersChoice`). */
export interface WorkersChoice {
  model: string;
  effort?: string;
}
/** One set of loop settings, as `loop.settings.get` and the setup card carry it. */
export interface LoopSettings {
  profile: string;
  /** `null`: the thread's own model. */
  implementer?: ModelChoice | null;
  workers: WorkersChoice;
  reviewer: ReviewerChoice;
  plan_check?: string;
  commit?: string;
  /** What a review loop commits: `match` (follow the thread) or `none`. */
  review_commit?: string;
  /** Who audits the sweep's rules: `thread` (the thread, with its own subagents) or `runs`
   * (one read-only reviewer run per rule, a few at once: one reviewer call per rule). */
  sweep?: string;
  [key: string]: Json;
}
export interface Preset {
  id: string;
  label: string;
  implementer: ModelChoice;
  workers: WorkersChoice;
  reviewer: ReviewerChoice;
}
/** What the hub found of one runtime on its own machine, or on a thread's remote box
 * (`loop.settings.get` `runtimes`). */
export interface RuntimeInfo {
  runtime: string;
  found: boolean;
  login: boolean;
  models: string[];
  efforts: string[];
  /** Accounts the runtime has, when the hub lists them. */
  accounts?: string[];
  /** Whether it runs subagents, when the hub says. */
  subagents?: boolean;
  /**
   * `false`: the hub does not probe its agent CLIs, so it does not know whether this one is
   * installed or logged in, and counts it as both (`found` and `login` are true).
   */
  probed?: boolean;
  /**
   * An ACP agent of a `[runtimes.<name>]` preset (`runtime` is its name): it can only review,
   * and only when `cannot_review` is unset.
   */
  acp?: boolean;
  /** An ACP preset's read-only mode, which its review runs ask the agent for. */
  review_mode?: string | null;
  /** Why an ACP preset can't review (`acp preset foo names no read-only mode`). */
  cannot_review?: string | null;
}
/** `loop.settings.get {project?, thread?}`. */
export interface SettingsGet {
  /** The remote box the runtimes were found on (a thread's), else null. */
  box?: string | null;
  settings?: LoopSettings | null;
  id?: string | null;
  source?: "project" | "workspace" | null;
  first_card: LoopSettings;
  projects: string[];
  presets: Preset[];
  profiles: string[];
  runtimes: RuntimeInfo[];
  warnings: string[];
  problems: string[];
}
export type SaveFor = "all" | "project";
/** The setup card's payload (`LoopSetup`, `loop.setup.get`'s `setup`). */
export interface LoopSetup {
  kind: string;
  first_run: boolean;
  why: string;
  settings: LoopSettings;
  save_for: SaveFor;
  project?: string;
  scope?: string;
  scope_summary?: string;
  overrides?: Json;
  warnings?: string[];
  problems?: string[];
  profile?: Json;
  /** An implementation loop's task, held until the card is answered; none: what the
   * conversation already settled. */
  task?: string | null;
  /** The implementer's runtime runs no subagents: the card has no Workers row (only when
   * true; a card raised before the hub said so shows the row). */
  no_workers?: boolean;
}
/** A commit a loop made, as the hub's check found it (twr-core `Commit`). */
export interface Commit {
  repo: string;
  sha: string;
}
/** A rule's audit run, when the hub audits the sweep (`sweep = "runs"`, twr-core
 * `AuditView`). */
export interface AuditView {
  /** 1-based: its run is `s<rule>`. */
  rule: number;
  name: string;
  status: "pending" | "running" | "answered" | "not_run";
  findings: number;
  /** A run of it failed; the next is its retry. */
  retried?: boolean;
  reason?: string | null;
}
/** A thread's running loop (`thread.loop`, twr-core `LoopState`). */
export interface LoopState {
  id: string;
  kind: string;
  profile: string;
  phase: string;
  tier?: string | null;
  round?: string | null;
  rounds: {
    id: string;
    findings: number;
    new_class: number;
    failed?: boolean;
  }[];
  ledger: number;
  verify: { done: number; cap: number };
  paused: boolean;
  stuck?: string | null;
  waiting_on: "thread" | "review" | "user";
  reviewer: { runtime: string; model: string; effort?: string | null };
  review_started_at?: string | null;
  outcome?: string | null;
  plan?: { sweep?: boolean; [key: string]: Json };
  /** An implementation loop: 0 before the plan turn answered, 1 once it did, 2 once the
   * plan check revised it. */
  plan_revision?: number;
  /** What the latest plan said of contracts. */
  contract_risk?: boolean | null;
  /** What it commits (`end`, `per-round`, `none`). */
  commit?: string;
  commits?: Commit[];
  commit_problem?: string | null;
  /** The step it waits for (`{step: "audits"}`: the sweep's audit runs). */
  wait?: { step?: string; [key: string]: Json };
  /** `sweep = "runs"`: the sweep's rules and their audit runs, in order. */
  audits?: AuditView[];
  [key: string]: Json;
}
/** The last loop that ended on a thread (`thread.last_loop`). */
export interface LastLoop {
  id: string;
  kind: string;
  outcome: string;
  ended_turn?: number | null;
  /** What it committed, as the hub's check found it: the repositories that were committed. */
  commits?: Commit[];
  /** Its commit's check failed twice: the other repositories' problems (`not committed`),
   * each starting with the repository's path, joined by `; `. A loop can have both: some
   * repositories committed, the others not. */
  commit_problem?: string | null;
}
/** One check of a loop's commit (`loop.get`'s `commit_checks`, twr-core `LoopCommitChecked`). */
export interface CommitCheck {
  commits?: Commit[];
  problems?: string[];
  /** What the check noted that is no problem (the thread wrote no `twr-commit` block). */
  notes?: string[];
  passed?: boolean;
  [key: string]: Json;
}

// ---------------------------------------------------------------------------------------
// The Review loop offer
// ---------------------------------------------------------------------------------------

/**
 * Whether a thread's card and its latest report offer a **Review loop** button: its turn
 * finished with changes no loop has reviewed. All of these hold: `[loops] offer_review` is
 * not false; the thread is a worker that is not parked, runs no loop and has no turn in
 * flight; it has finished a turn; its git state shows changes (files not committed, or
 * commits ahead of its base); and no loop ended since its last turn.
 *
 * A thread someone stopped (`stopped` and not `stopped_by_hub`) is not: the hub refuses a
 * loop there (`send it a message to start it again first`). A thread stopped only by the
 * hub's own shutdown is: it goes on (and a loop with it) as soon as it is sent something.
 */
export function offersReview(
  thread: Thread | undefined,
  offerReview: boolean | undefined,
  attention?: Pick<Attention, "thread" | "loop_setup">[],
): boolean {
  if (!thread || offerReview === false) return false;
  // Its setup card is open: that is where the loop starts, not here.
  if (attention?.some((a) => a.thread === thread.id && a.loop_setup === true))
    return false;
  if (thread.is_orchestrator || thread.parked) return false;
  if (["parked", "attached"].includes(thread.status)) return false;
  if (thread.loop) return false;
  if (stoppedBySomeone(thread)) return false;
  if (thread.current_turn != null) return false;
  const lastTurn = Number(thread.last_turn?.turn);
  if (!Number.isFinite(lastTurn) || lastTurn <= 0) return false;
  const git = thread.git;
  const changed =
    !!git && (Number(git.dirty_files) > 0 || Number(git.base_ahead) > 0);
  if (!changed) return false;
  const last = thread.last_loop as LastLoop | undefined | null;
  if (last && Number(last.ended_turn ?? 0) >= lastTurn) return false;
  return true;
}

/** Whether a thread counts as stopped because someone stopped it, not only the hub's own
 * shutdown: the hub refuses to start a loop on it (`loop.request`) until it is sent
 * something. */
export const stoppedBySomeone = (thread: Thread | undefined) =>
  !!thread?.stopped && !thread.stopped_by_hub;

/** `config.show`'s `loops.offer_review`: true unless the hub says false. */
export function offerReviewSetting(config: Json): boolean {
  return config?.loops?.offer_review !== false;
}

// ---------------------------------------------------------------------------------------
// The setup card
// ---------------------------------------------------------------------------------------

/** The setup card's options, as the hub raises them (twr-core `LOOP_SETUP_START`,
 * `LOOP_SETUP_RUN_WITHOUT`, `LOOP_SETUP_CANCEL`): `run without a loop` only on a card that
 * holds a task, between the other two. */
export const SETUP_START = "start and save these settings";
export const SETUP_RUN_WITHOUT = "run without a loop";
export const SETUP_CANCEL = "cancel";

export interface SetupSummary {
  /** The first line: what the card asks. */
  title: string;
  /** `Label   value` rows, in order. A value the hub wrote on several lines (a held task)
   * keeps them, joined by newlines. */
  rows: [string, string][];
  warnings: string[];
  /** What has to change before the loop can start (`Change first: …`). */
  problems: string[];
  /** Lines that are none of these, kept as they are. */
  other: string[];
}

/**
 * Reads a setup card's summary: a sentence, then `Label   value` rows (a label, then two or
 * more spaces), then `Warning: …` and `Change first: …` lines. An indented line right after
 * a row goes on with that row's value (the task's further lines and `(+N more lines)`). A
 * line it can't place stays in `other`, so nothing the hub wrote is lost.
 */
export function parseSetupSummary(summary: string): SetupSummary {
  const lines = String(summary || "")
    .split(/\r?\n/)
    .map((line) => line.trimEnd());
  const out: SetupSummary = {
    title: "",
    rows: [],
    warnings: [],
    problems: [],
    other: [],
  };
  let first = true;
  // Whether the line before was a row (or its continuation), which an indented line goes on.
  let inRow = false;
  for (const line of lines) {
    if (!line.trim()) {
      inRow = false;
      continue;
    }
    if (first) {
      out.title = line.trim();
      first = false;
      continue;
    }
    const warning = /^\s*Warning:\s*(.*)$/i.exec(line);
    const problem = /^\s*Change first:\s*(.*)$/i.exec(line);
    const continued = inRow && /^\s{2,}\S/.test(line);
    const row = /^\s*([A-Za-z][A-Za-z ]{0,30}?)\s{2,}(\S.*)$/.exec(line);
    inRow = false;
    if (warning) out.warnings.push(warning[1].trim());
    else if (problem) out.problems.push(problem[1].trim());
    else if (continued) {
      const last = out.rows[out.rows.length - 1];
      last[1] = `${last[1]}\n${line.trim()}`;
      inRow = true;
    } else if (row) {
      out.rows.push([row[1].trim(), row[2].trim()]);
      inRow = true;
    } else out.other.push(line.trim());
  }
  return out;
}

/** The kind of loop a setup card is for, from what the hub wrote: an implementation loop's
 * card says so in its first line and has a Task row. */
export function setupKind(parsed: SetupSummary): "implement" | "review" {
  return /implementation loop/i.test(parsed.title) ||
    parsed.rows.some(([label]) => label === "Task")
    ? "implement"
    : "review";
}

/** One answer of a setup card, as its button says it. */
export interface SetupOption {
  /** What the hub takes (`attention.respond` `answer`). */
  answer: string;
  label: string;
  /** Start and save, while the card lists something to change first, can't be pressed. */
  disabled: boolean;
  /** Why not, when not. */
  why?: string;
}

/** A setup card's answers, in the order the hub raised them, as buttons say them; one the
 * apps don't know shows as the hub wrote it. */
export function setupOptions(
  options: string[],
  problems: string[] = [],
): SetupOption[] {
  return options.map((answer) => {
    if (answer === SETUP_START)
      return {
        answer,
        label: "Start and save",
        disabled: problems.length > 0,
        ...(problems.length ? { why: "Change what the card lists first" } : {}),
      };
    if (answer === SETUP_RUN_WITHOUT)
      return { answer, label: "Run without a loop", disabled: false };
    if (answer === SETUP_CANCEL)
      return { answer, label: "Cancel", disabled: false };
    return {
      answer,
      label: answer.charAt(0).toUpperCase() + answer.slice(1),
      disabled: false,
    };
  });
}

/** Whether `item` is a loop's setup card, what it is for, and what may be pressed. */
export function setupCardState(item: {
  loop_setup?: boolean;
  summary: string;
  options?: string[];
}) {
  const parsed = parseSetupSummary(item.summary);
  const options = item.options || [];
  const kind = setupKind(parsed);
  const task = parsed.rows.find(([label]) => label === "Task")?.[1];
  return {
    parsed,
    kind,
    /** The task as the summary shows it (its first lines); `loop.setup.get` has it whole. */
    task,
    options: setupOptions(options, parsed.problems),
    canStart: options.includes(SETUP_START) && parsed.problems.length === 0,
    canRunWithout: options.includes(SETUP_RUN_WITHOUT),
    canCancel: options.includes(SETUP_CANCEL),
  };
}

/** A request in one line, where a card or list names it: a setup card's first line says
 * which loop it is for; its other lines are rows (a held task among them). */
export function requestLine(item: { summary?: string; loop_setup?: boolean }) {
  const summary = item.summary || "";
  return item.loop_setup
    ? summary.split("\n").find((line) => line.trim()) || summary
    : summary;
}

/** The setup card's heading: the kind of loop it is for. */
export const setupHeading = (kind: string) =>
  kind === "implement" ? "Implementation loop setup" : "Review loop setup";

// ---------------------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------------------

export const modelWords = (m: {
  runtime: string;
  account?: string;
  model?: string;
  effort?: string;
}) =>
  [
    m.runtime + (m.account ? `:${m.account}` : ""),
    m.model || "its default model",
    ...(m.effort ? [m.effort] : []),
  ].join(" · ");

/**
 * The line under the inspector's Review loop button: the settings a loop would run on, or,
 * before any are saved, that the first one asks.
 */
export function settingsLine(got: SettingsGet | undefined): string {
  if (!got) return "";
  const s = got.settings;
  if (!s) return "first run: you will be asked to choose models";
  const implementer = s.implementer
    ? modelWords(s.implementer)
    : "the thread's own model";
  const from =
    got.source === "project" ? "this project's settings" : "saved settings";
  return `${s.profile} · implementer ${implementer} · reviewer ${modelWords(s.reviewer)} (${from})`;
}

/**
 * What a loop started on, from `loop.get`'s `started`: the profile, the implementer, the
 * reviewer and whose settings they are, as the hub's line to the orchestrator says it
 * (`high · implementer claude-opus-5-5 high · reviewer codex gpt-6.1-sol (saved settings)`).
 * Empty when `started` says nothing.
 */
export function startedWords(
  started: Json,
  thread?: Pick<
    Thread,
    "runtime" | "model" | "model_actual" | "effort" | "project"
  >,
): string {
  const s = started?.settings as LoopSettings | undefined;
  if (!s || typeof s !== "object" || !s.reviewer) return "";
  const own = [thread?.model_actual || thread?.model, thread?.effort]
    .filter(Boolean)
    .join(" ");
  const implementer = s.implementer
    ? [s.implementer.model, s.implementer.effort].filter(Boolean).join(" ")
    : `${own || thread?.runtime || "its own model"} (the thread's own)`;
  const from =
    started.settings_from === "saved"
      ? started.settings_of === "project"
        ? `${thread?.project || "the project"}'s settings`
        : "saved settings"
      : "settings chosen on its setup card";
  return `${s.profile} · implementer ${implementer} · reviewer ${s.reviewer.runtime} ${s.reviewer.model} (${from})`;
}

/** The phases a loop of `kind` goes through, in order (twr-core `Phase`). */
export function phasesOf(kind: string): string[] {
  return kind === "implement"
    ? ["plan", "plan_check", "implement", "ladder", "sweep", "verify", "report"]
    : ["scope", "ladder", "sweep", "verify", "report"];
}
export const phaseWords = (phase: string) => phase.replace(/_/g, " ");

/** The plan check's round (twr-core `contract::PLAN_CHECK_ROUND`). */
export const PLAN_CHECK_ROUND = "p1";

/**
 * The phases running loop `state` passed over without running them: an implementation
 * loop's plan check when it did not run, the verification when
 * the ladder's end skipped it (`transition.verify_skipped`: the last tier's round found
 * nothing at the verification's effort, so v1 could only repeat it), and the sweep once the
 * ladder ended without one (the profile has none, the ladder fixed nothing, or the loop
 * skipped to verify).
 */
export function skippedPhases(state: LoopState): string[] {
  const out: string[] = [];
  // An implementation loop's plan check runs only when its setting says so (`always`, or
  // `contracts` and a plan with contract risk): past it with no round `p1`, it did not run.
  if (
    state.kind === "implement" &&
    phasesOf("implement").indexOf(state.phase) >
      phasesOf("implement").indexOf("plan_check") &&
    !state.rounds.some((r) => r.id === PLAN_CHECK_ROUND)
  )
    out.push("plan_check");
  const transition = state.transition as
    { reason?: string; verify_skipped?: boolean } | null | undefined;
  if (!transition) return out;
  const swept =
    !!state.plan?.sweep &&
    Number(state.fixed || 0) > 0 &&
    transition.reason !== "skipped";
  if (!swept && state.phase !== "sweep") out.push("sweep");
  if (transition.verify_skipped) out.push("verify");
  return out;
}

/** What a running loop waits for, in words. */
export function waitingWords(
  l: Pick<LoopState, "waiting_on" | "paused" | "stuck"> &
    Partial<Pick<LoopState, "wait">>,
) {
  if (l.stuck) return `stuck: ${l.stuck}`;
  if (l.waiting_on === "review" && l.wait?.step === "audits")
    // Paused, the audits in flight finish; none starts until Resume.
    return l.paused
      ? "paused: the audits in flight finish first"
      : "waiting on the audits";
  if (l.waiting_on === "review")
    // Paused, the review in flight still finishes; the loop then waits for Resume.
    return l.paused
      ? "paused: the review in flight finishes first"
      : "waiting on the review";
  if (l.waiting_on === "user")
    return l.paused ? "paused: waiting on you" : "waiting on you";
  return l.paused ? "paused after the thread's turn" : "waiting on the thread";
}

/** A round's line in the inspector. The round whose review run is in flight (the last one,
 * while the loop waits on the review) has found nothing yet: it says so, rather than "0
 * findings", which reads as a clean round. */
export function roundWords(
  round: LoopState["rounds"][number],
  l: Pick<LoopState, "rounds" | "waiting_on">,
) {
  if (round.failed) return "review run failed";
  if (l.waiting_on === "review" && l.rounds.at(-1)?.id === round.id)
    return "under review";
  return `${round.findings} finding${round.findings === 1 ? "" : "s"} · ${round.new_class} new class`;
}

/** A rule's audit in the inspector: what its run found, or where it is. */
export function auditWords(a: AuditView): string {
  switch (a.status) {
    case "answered":
      return a.findings === 0
        ? "no place breaks it"
        : `${a.findings} finding${a.findings === 1 ? "" : "s"}`;
    case "running":
      return a.retried ? "auditing again (its run failed once)" : "auditing";
    case "not_run":
      return `not run${a.reason ? `: ${a.reason}` : ""}`;
    default:
      return a.retried ? "waiting to run again" : "waiting for a slot";
  }
}

/** A thread's card while its loop's sweep audits are its only background work: how many
 * rules are being audited, of all (`auditing 2 of 5 rules`); else undefined. */
export function auditingWords(thread: Json): string | undefined {
  const l = thread?.loop as LoopState | undefined;
  if (!l || l.wait?.step !== "audits" || !Array.isArray(l.audits)) return;
  const tasks = Object.values(
    (thread?.background_tasks || {}) as Record<string, Json>,
  );
  if (!tasks.length || !tasks.every((t) => t?.review)) return;
  return `auditing ${tasks.length} of ${l.audits.length} rule${l.audits.length === 1 ? "" : "s"}`;
}

/** How a loop ended, in words. */
export function outcomeWords(outcome: string | null | undefined) {
  switch (outcome) {
    case "clean":
      return "clean";
    case "ledger_fixed_point":
      return "only known findings left";
    case "cap_expired":
      return "verification cap reached with findings left";
    case "stopped":
      return "stopped";
    case "failed":
      return "failed";
    default:
      return outcome ? phaseWords(outcome) : "";
  }
}

/** The short form of a commit's sha. */
export const shortSha = (sha: string) => String(sha || "").slice(0, 7);

/** What a loop committed, in words: `committed abc1234` (with the repository's name when
 * there are several), then, when other repositories were not committed, `not committed:
 * <why>` (`committed abc1234 · not committed: /w/web: …`); nothing when it asked no commit.
 * Never `not committed` alone for a loop that committed something. */
export function commitWords(
  l: Pick<LastLoop, "commits" | "commit_problem">,
): string {
  const commits = l.commits || [];
  const repo = (path: string) => path.split(/[\\/]/).filter(Boolean).at(-1);
  const parts: string[] = [];
  if (commits.length)
    parts.push(
      `committed ${commits
        .map((c) =>
          commits.length > 1 && repo(c.repo)
            ? `${shortSha(c.sha)} (${repo(c.repo)})`
            : shortSha(c.sha),
        )
        .join(", ")}`,
    );
  if (l.commit_problem) parts.push(`not committed: ${l.commit_problem}`);
  return parts.join(" · ");
}

/** The notes of a loop's last commit check (`loop.get`'s `commit_checks`): what it noted that
 * is no problem. */
export function commitNotes(detail: Json): string[] {
  const checks: CommitCheck[] = Array.isArray(detail?.commit_checks)
    ? detail.commit_checks
    : [];
  const notes = checks.at(-1)?.notes;
  return Array.isArray(notes)
    ? notes.filter((n): n is string => typeof n === "string" && !!n.trim())
    : [];
}

/** `loop.report {thread}` or `{id}`: a loop's `report.md` as text, for a device that cannot
 * read the hub's files (a phone, a remote desktop). */
export interface LoopReport {
  thread: string;
  thread_name: string;
  id: string;
  kind?: string | null;
  running: boolean;
  outcome?: string | null;
  /** Where the hub wrote it, on the hub's machine. */
  path?: string | null;
  written: boolean;
  /** The report, or its first 256 KiB with a marker line at the end (`truncated`). */
  text?: string | null;
  bytes?: number | null;
  truncated: boolean;
  /** Why there is no text: a loop still running, one that ended stopped or failed, a file
   * the hub can't read. */
  note?: string | null;
}

/** The report's dialog: its title, the line under it, its text or why there is none. */
export function reportView(r: LoopReport, name?: string) {
  const kind = kindWords(r.kind ?? undefined);
  const how = r.running ? "running" : outcomeWords(r.outcome);
  const size =
    typeof r.bytes === "number" && r.bytes >= 0 ? sizeWords(r.bytes) : "";
  const text = typeof r.text === "string" ? r.text : undefined;
  return {
    title: `${kind.charAt(0).toUpperCase()}${kind.slice(1)} report · ${
      r.thread_name || name || "twr"
    }`,
    subtitle: [how, size].filter(Boolean).join(" · "),
    text,
    note:
      text === undefined
        ? r.note ||
          (r.running
            ? "The loop is still running: its report is written when it ends."
            : "This loop wrote no report.")
        : undefined,
    path: typeof r.path === "string" && r.path ? r.path : undefined,
  };
}

/** A size in bytes, in words, as attachments say it: `812 bytes`, `12.3 KiB`, `1.2 MiB`. */
export function sizeWords(bytes: number): string {
  if (bytes < 1024) return `${bytes} byte${bytes === 1 ? "" : "s"}`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

/** `review loop` or `implementation loop`. */
export const kindWords = (kind: string | undefined) =>
  kind === "implement" ? "implementation loop" : "review loop";

/** Rounds a loop ran, as a notification counts them: the plan check's `p1` is not one. */
export const countedRounds = (rounds: { id: string }[] | undefined) =>
  (rounds || []).filter((r) => r.id !== PLAN_CHECK_ROUND).length;

/**
 * How a loop ended, in a line: `review loop clean after 6 rounds`; an implementation loop's
 * adds its commit, or that it was not committed: `implementation loop clean after 3 rounds ·
 * committed abc1234`. `rounds` when it is known.
 */
export function endedWords(last: LastLoop, rounds?: number): string {
  const how = outcomeWords(last.outcome);
  const commit = commitWords(last);
  return [
    `${kindWords(last.kind)}${how ? ` ${how}` : " ended"}${
      rounds && rounds > 0
        ? ` after ${rounds} round${rounds === 1 ? "" : "s"}`
        : ""
    }`,
    ...(commit ? [commit] : []),
  ].join(" · ");
}

/** Whether a thread's last loop ended at its latest turn: its ended line still describes
 * what the card's last report says. */
export function endedLately(thread: Thread | undefined): LastLoop | undefined {
  if (!thread || thread.loop) return undefined;
  const last = thread.last_loop as LastLoop | undefined | null;
  if (!last?.id) return undefined;
  const lastTurn = Number(thread.last_turn?.turn ?? 0);
  if (Number(last.ended_turn ?? 0) < lastTurn) return undefined;
  return last;
}

/**
 * The loops that ended since `known` (thread id → the id of its last loop, as last seen), as
 * the words a screen reader hears (`api's review loop ended: clean`; an implementation loop
 * adds its commit: `…: clean; committed abc1234`, `not committed: …`, or both), and the new
 * `known`.
 * A thread seen for the first time is known, not news.
 */
export function endedLoops(
  known: Map<string, string> | undefined,
  threads: { id: string; name: string; last_loop?: Json }[],
): { said: string[]; known: Map<string, string> } {
  const next = new Map<string, string>();
  const said: string[] = [];
  for (const t of threads) {
    const last = t.last_loop as LastLoop | undefined | null;
    const id = last?.id || "";
    next.set(t.id, id);
    if (!known || !known.has(t.id) || !id || known.get(t.id) === id) continue;
    const kind = last!.kind === "implement" ? "implementation" : "review";
    const how = outcomeWords(last!.outcome);
    const commit = commitWords(last!);
    said.push(
      `${t.name}'s ${kind} loop ended${how ? `: ${how}` : ""}${commit ? `; ${commit}` : ""}`,
    );
  }
  return { said, known: next };
}

/** A loop that started, as a screen reader hears it and the notice says it. */
export function startedNotice(name: string, kind: string, line?: string) {
  return `${name} started ${kind === "implement" ? "an implementation loop" : "a review loop"}${
    line ? `: ${line}` : ""
  }.`;
}

/**
 * Whether **Skip to verify** can be pressed, and why not: an implementation loop has built
 * nothing to verify before its ladder (the hub refuses it in `plan`, `plan_check` and
 * `implement`); a skip already asked for waits for the round's end. `show: false`: the
 * phase is past the ladder, where it means nothing.
 */
export function skipToVerify(state: { phase: string; skip?: boolean }): {
  show: boolean;
  enabled: boolean;
  why?: string;
} {
  if (["plan", "plan_check", "implement"].includes(state.phase))
    return {
      show: true,
      enabled: false,
      why: "Nothing has been built to verify yet: Skip to verify is available from the ladder on.",
    };
  if (!["scope", "ladder"].includes(state.phase) || state.skip)
    return { show: false, enabled: false };
  return { show: true, enabled: true };
}

/** An implementation loop's plan, as `loop.get`'s `plans` lists each revision. */
export interface PlanInfo {
  revision: number;
  contract_risk: boolean;
  reason: string;
  path?: string | null;
}

/** The plan in words: `revision 2 · contract risk yes: a migration`. */
export function planWords(plan: PlanInfo): string {
  return `revision ${plan.revision} · contract risk ${plan.contract_risk ? "yes" : "no"}${
    plan.reason ? `: ${plan.reason}` : ""
  }`;
}

/** The implement turn's result, as `loop.get`'s `implemented` lists each block read:
 * `done · gates pass: cargo test`, and whether the hub took it. */
export function implementedWords(i: {
  status?: string;
  gates?: { ran?: string[]; result?: string };
  accepted?: boolean;
}): string {
  const ran = i.gates?.ran?.length ? `: ${i.gates.ran.join(", ")}` : "";
  return `${i.status || "?"} · gates ${i.gates?.result || "not run"}${ran}${
    i.accepted === false
      ? " (sent back: gates must be green before review)"
      : ""
  }`;
}

// ---------------------------------------------------------------------------------------
// Who may do what
// ---------------------------------------------------------------------------------------

export interface LoopAccess {
  /** Start a loop, control one, answer its setup card, save settings. */
  act: boolean;
  /** Why not, when not. */
  why?: string;
}

/**
 * What a device may do with loops: `read` devices see everything and do nothing; every
 * action needs `operate` and a live connection (nothing goes to the outbox).
 */
export function loopAccess({
  scopes,
  connected,
}: {
  scopes?: string[];
  connected: boolean;
}): LoopAccess {
  if (scopes && !scopes.includes("operate"))
    return { act: false, why: "This device has read-only access." };
  if (!connected)
    return {
      act: false,
      why: "Not connected to the live hub. Loop actions need a live connection.",
    };
  return { act: true };
}

// ---------------------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------------------

export type LoopControlAction = "pause" | "resume" | "stop" | "skip_to_verify";

/** `/review`'s parsed arguments (`desktop.parse_slash` kind `Review`). */
export interface ReviewArgs {
  thread?: string;
  setup?: boolean;
  scope?: string;
  profile?: string;
  reviewer?: string;
  implementer?: string;
  workers?: string;
  commit?: string;
}
/** `/loop`'s parsed arguments (`desktop.parse_slash` kind `Loop`). */
export interface LoopArgs {
  thread?: string;
  action: LoopControlAction | "settings" | "status";
  project?: string;
}

/** `loop.request`'s parameters for a review loop. */
export interface ReviewRequest {
  thread: string;
  kind: "review";
  setup?: boolean;
  scope?: string;
  overrides?: {
    profile?: string;
    implementer?: string;
    workers?: string;
    reviewer?: string;
    commit?: string;
  };
}

/** The `loop.request` a `/review` asks for, on `chosen` when it names no thread. */
export function reviewRequest(
  args: ReviewArgs | undefined,
  chosen: string | undefined,
): ReviewRequest {
  const a = args || {};
  const thread = a.thread || chosen;
  if (!thread)
    throw new Error("Choose a thread, or name it after /review, to review it.");
  const overrides: NonNullable<ReviewRequest["overrides"]> = {};
  for (const key of [
    "profile",
    "implementer",
    "workers",
    "reviewer",
    "commit",
  ] as const)
    if (typeof a[key] === "string" && a[key]!.trim())
      overrides[key] = a[key]!.trim();
  return {
    thread,
    kind: "review",
    ...(a.setup ? { setup: true } : {}),
    ...(a.scope?.trim() ? { scope: a.scope.trim() } : {}),
    ...(Object.keys(overrides).length ? { overrides } : {}),
  };
}

/** `/implement`'s parsed arguments (`desktop.parse_slash` kind `Implement`): the words that
 * are no flag, in order (perhaps a thread first, then the task), and the run's flags. */
export interface ImplementArgs {
  words: string[];
  setup?: boolean;
  profile?: string | null;
  reviewer?: string | null;
  implementer?: string | null;
  workers?: string | null;
  commit?: string | null;
  plan_check?: string | null;
}

/** `loop.request`'s parameters for an implementation loop. */
export interface ImplementRequest {
  thread: string;
  kind: "implement";
  /** None: the task is what the conversation already settled. */
  task?: string;
  setup?: boolean;
  overrides?: {
    profile?: string;
    implementer?: string;
    workers?: string;
    reviewer?: string;
    commit?: string;
    plan_check?: string;
  };
}

/**
 * Where `/implement`'s words go, as twr-core's `slash::implement_target` splits them for
 * every client: in a thread's conversation (`dive`) the thread is that one and every word is
 * the task's; elsewhere a first word that names a live thread (`namesThread`) is the thread,
 * else the thread is the selected card's, and the other words are the task. The task's words
 * are joined by single spaces; none means the task is what the conversation already settled.
 */
export function implementTarget(
  words: string[],
  dive: boolean,
  namesThread: (word: string) => boolean,
): { thread?: string; task?: string } {
  const [first, ...rest] = words;
  const named = !dive && first !== undefined && namesThread(first);
  const task = (named ? rest : words)
    .map((w) => w.trim())
    .filter(Boolean)
    .join(" ");
  return {
    ...(named ? { thread: first } : {}),
    ...(task ? { task } : {}),
  };
}

/** A name, alias or id of a thread in `threads` (the view lists no deleted thread), as the
 * TUI resolves `/implement`'s first word. */
export const namesThread = (
  threads: { id: string; name: string; aliases?: string[] }[],
  word: string,
) =>
  threads.some(
    (t) => t.id === word || t.name === word || (t.aliases || []).includes(word),
  );

/**
 * The `loop.request` an `/implement` asks for: on the thread it names, else the one whose
 * conversation is shown (`dive`), else the selected card's (`chosen`).
 */
export function implementRequest(
  args: ImplementArgs | undefined,
  {
    dive,
    chosen,
    threads,
  }: {
    /** The thread whose conversation is shown, when it is a thread's. */
    dive?: string;
    /** The selected card's thread, else the one shown. */
    chosen?: string;
    threads: { id: string; name: string; aliases?: string[] }[];
  },
): ImplementRequest {
  const a = args || { words: [] };
  const target = implementTarget(a.words || [], !!dive, (w) =>
    namesThread(threads, w),
  );
  const thread = target.thread || dive || chosen;
  if (!thread)
    throw new Error(
      "No thread to run it in: select a thread, open its conversation, or name it first (/implement <thread> <task>).",
    );
  const overrides: NonNullable<ImplementRequest["overrides"]> = {};
  for (const key of [
    "profile",
    "implementer",
    "workers",
    "reviewer",
    "commit",
    "plan_check",
  ] as const) {
    const value = a[key];
    if (typeof value === "string" && value.trim())
      overrides[key] = value.trim();
  }
  return {
    thread,
    kind: "implement",
    ...(target.task ? { task: target.task } : {}),
    ...(a.setup ? { setup: true } : {}),
    ...(Object.keys(overrides).length ? { overrides } : {}),
  };
}

/** The most bytes a loop's task may have (twr-core `settings::MAX_TASK_BYTES`). */
export const MAX_TASK_BYTES = 16 * 1024;

/**
 * What **Send as implementation loop** sends for a composer's `draft`: the message is the
 * task, as typed (`//` escapes a leading `/`, as Send does). A command or a shell line is no
 * task. An empty draft asks for what the conversation already settled, and `confirm` says so
 * first: nothing was typed.
 */
export function sendAsLoop(draft: string): {
  task?: string;
  confirm: boolean;
} {
  const text = draft.startsWith("//") ? draft.slice(1) : draft;
  if (draft.startsWith("/") && !draft.startsWith("//"))
    throw new Error(
      "A command is no task: send it with Send, or write what to build (start it with // to send a leading /).",
    );
  if (draft.startsWith("!") && !draft.startsWith("!!!"))
    throw new Error(
      "A shell command is no task: send it with Send, or write what to build.",
    );
  const task = text.trim();
  if (new TextEncoder().encode(task).length > MAX_TASK_BYTES)
    throw new Error(
      "The task is longer than 16 KiB: put the details in a file and name it in the task.",
    );
  return task ? { task, confirm: false } : { confirm: true };
}

/**
 * The New thread form's request: `thread.new`'s parameters, with `loop: {kind: "implement",
 * task}` when **Run as an implementation loop** is on (the first prompt is the task, and is
 * required), and otherwise the first prompt to send once the thread exists (`firstPrompt`).
 * `prompt`, `implement` and `loop_setup` are the form's own keys, never sent as they are.
 */
export function newThreadRequest(params: Record<string, Json>): {
  params: Record<string, Json>;
  firstPrompt?: string;
} {
  const {
    prompt,
    implement,
    loop_setup: setup,
    ...rest
  } = params as Record<string, Json>;
  const text = typeof prompt === "string" ? prompt.trim() : "";
  if (implement) {
    if (!text)
      throw new Error(
        "The first prompt is the implementation loop's task: write it, or turn off Run as an implementation loop.",
      );
    if (new TextEncoder().encode(text).length > MAX_TASK_BYTES)
      throw new Error(
        "The task is longer than 16 KiB: put the details in a file and name it in the task.",
      );
    return {
      params: {
        ...rest,
        loop: {
          kind: "implement",
          task: text,
          ...(setup ? { setup: true } : {}),
        },
      },
    };
  }
  return { params: rest, ...(text ? { firstPrompt: text } : {}) };
}

/** `/new … --implement`'s parsed arguments, as the New thread form's values. */
export function newThreadValues(args: Json): Record<string, Json> {
  const { implement, setup, task, ...rest } = args || {};
  return {
    ...Object.fromEntries(
      Object.entries(rest).filter(([, v]) => v !== null && v !== undefined),
    ),
    ...(implement
      ? {
          implement: true,
          prompt: task || "",
          ...(setup ? { loop_setup: true } : {}),
        }
      : {}),
  };
}

/** What `thread.new`'s answer says of the loop it asked for: started, its setup card, or
 * refused after the thread was made (`{status: "error", code, message}`). */
export function newThreadLoop(
  result: Json,
):
  | { status: "started" | "needs_setup"; attn_id?: string }
  | { status: "error"; message: string }
  | undefined {
  const l = result?.loop;
  if (!l || typeof l !== "object" || typeof l.status !== "string") return;
  if (l.status === "error")
    return {
      status: "error",
      message: String(l.message || l.code || "the loop could not start"),
    };
  if (l.status === "started" || l.status === "needs_setup")
    return {
      status: l.status,
      ...(typeof l.attn_id === "string" ? { attn_id: l.attn_id } : {}),
    };
}

/** What a `/loop` does: a control of a thread's loop, the editor, or its progress. */
export type LoopSlash =
  | { kind: "control"; thread: string; action: LoopControlAction }
  | { kind: "saved"; project?: string }
  | { kind: "status"; thread: string };

export function loopSlash(
  args: LoopArgs | undefined,
  chosen: string | undefined,
): LoopSlash {
  const action = args?.action || "status";
  if (action === "settings")
    return {
      kind: "saved",
      ...(args?.project?.trim() ? { project: args.project.trim() } : {}),
    };
  const thread = args?.thread || chosen;
  if (!thread)
    throw new Error("Choose a thread, or name it after /loop, for its loop.");
  if (action === "status") return { kind: "status", thread };
  return { kind: "control", thread, action };
}

// ---------------------------------------------------------------------------------------
// The settings editor
// ---------------------------------------------------------------------------------------

/** Where the editor reads and saves: a setup card's draft, or saved settings. */
export type EditorSource =
  | { kind: "card"; attnId: string; thread?: string; project?: string }
  | { kind: "saved"; project?: string };

export interface EditorState {
  settings: LoopSettings;
  saveFor: SaveFor;
  /** A setup card's revision, as shown. */
  revision?: number;
  /** The project `Save for: this project` saves for, if any. */
  project?: string;
}

export type EditorChange =
  | { kind: "preset"; preset: Preset }
  | { kind: "implementer"; own: true }
  | {
      kind: "implementer";
      own?: false;
      runtime?: string;
      model?: string;
      effort?: string;
    }
  | { kind: "workers"; model?: string; effort?: string }
  | { kind: "reviewer"; runtime?: string; model?: string; account?: string }
  | { kind: "profile"; profile: string }
  | { kind: "review_commit"; value: string }
  /** `""`: the profile's plan check. */
  | { kind: "plan_check"; value: string }
  | { kind: "commit"; value: string }
  | { kind: "sweep"; value: string }
  | { kind: "save_for"; value: SaveFor };

const defined = <T extends object>(value: T): T =>
  Object.fromEntries(
    Object.entries(value).filter(([, v]) => v !== undefined && v !== ""),
  ) as T;

/** The first model `runtime` reports, for a runtime just chosen. */
function firstModel(runtime: string, runtimes: RuntimeInfo[]) {
  return runtimes.find((r) => r.runtime === runtime)?.models[0] || "";
}

/** The editor's state after `change`. A runtime change keeps the model only when the new
 * runtime reports it; workers follow a named implementer's runtime. */
export function editSettings(
  state: EditorState,
  change: EditorChange,
  runtimes: RuntimeInfo[] = [],
): EditorState {
  const s = state.settings;
  switch (change.kind) {
    case "preset":
      return {
        ...state,
        settings: {
          ...s,
          implementer: { ...change.preset.implementer },
          workers: { ...change.preset.workers },
          reviewer: { ...change.preset.reviewer },
        },
      };
    case "implementer": {
      if (change.own)
        return { ...state, settings: { ...s, implementer: null } };
      const old = s.implementer;
      const runtime = change.runtime ?? old?.runtime ?? "claude-code";
      const sameRuntime = runtime === old?.runtime;
      const reported = runtimes.find((r) => r.runtime === runtime)?.models;
      const model =
        change.model ??
        (sameRuntime || !reported?.length || reported.includes(old?.model || "")
          ? old?.model
          : undefined) ??
        firstModel(runtime, runtimes);
      const effort =
        change.effort !== undefined
          ? change.effort || undefined
          : sameRuntime
            ? old?.effort
            : undefined;
      return {
        ...state,
        settings: {
          ...s,
          implementer: defined({
            runtime,
            account: sameRuntime ? old?.account : undefined,
            model: model || "",
            effort,
          }) as ModelChoice,
        },
      };
    }
    case "workers":
      return {
        ...state,
        settings: {
          ...s,
          workers: defined({
            model: change.model ?? s.workers.model,
            effort:
              change.effort !== undefined
                ? change.effort || undefined
                : s.workers.effort,
          }) as WorkersChoice,
        },
      };
    case "reviewer": {
      const old = s.reviewer;
      const runtime = change.runtime ?? old.runtime;
      const sameRuntime = runtime === old.runtime;
      const reported = runtimes.find((r) => r.runtime === runtime)?.models;
      const model =
        change.model ??
        (sameRuntime || !reported?.length || reported.includes(old.model)
          ? old.model
          : firstModel(runtime, runtimes));
      const account =
        change.account !== undefined
          ? change.account || undefined
          : sameRuntime
            ? old.account
            : undefined;
      return {
        ...state,
        settings: {
          ...s,
          reviewer: defined({ runtime, account, model }) as ReviewerChoice,
        },
      };
    }
    case "profile":
      return { ...state, settings: { ...s, profile: change.profile } };
    case "review_commit":
      return { ...state, settings: { ...s, review_commit: change.value } };
    case "plan_check": {
      // No value is the profile's: the hub reads a missing `plan_check` as that.
      const { plan_check: _, ...rest } = s;
      return {
        ...state,
        settings: change.value
          ? { ...rest, plan_check: change.value }
          : (rest as LoopSettings),
      };
    }
    case "commit":
      return { ...state, settings: { ...s, commit: change.value } };
    case "sweep":
      return { ...state, settings: { ...s, sweep: change.value } };
    case "save_for":
      return { ...state, saveFor: change.value };
  }
}

/** The preset the settings match, if any (the segmented control's pressed one). */
export function matchingPreset(
  settings: LoopSettings,
  presets: Preset[],
): string | undefined {
  const same = (a: Json, b: Json) =>
    JSON.stringify(defined({ ...(a || {}) })) ===
    JSON.stringify(defined({ ...(b || {}) }));
  return presets.find(
    (p) =>
      !!settings.implementer &&
      same(p.implementer, settings.implementer) &&
      same(p.workers, settings.workers) &&
      same(p.reviewer, settings.reviewer),
  )?.id;
}

/** Whether `runtime` runs subagents (workers): what the hub says, else Claude Code. */
export function hasSubagents(
  runtime: string | undefined,
  runtimes: RuntimeInfo[],
) {
  if (!runtime) return false;
  const found = runtimes.find((r) => r.runtime === runtime);
  if (typeof found?.subagents === "boolean") return found.subagents;
  return runtime === "claude-code";
}

/**
 * Whether a runtime can be chosen, and why not. One the hub does not probe for (`probed:
 * false`) can: the hub does not know, and `note` says so.
 */
export function runtimeUsable(r: RuntimeInfo | undefined): {
  usable: boolean;
  why?: string;
  note?: string;
} {
  if (!r) return { usable: false, why: "not known to the hub" };
  if (r.probed === false) return { usable: true, note: "not checked" };
  if (!r.found) return { usable: false, why: "not installed" };
  if (!r.login) return { usable: false, why: "not logged in" };
  return { usable: true };
}

/**
 * Whether a runtime can be the reviewer, and why not: an ACP preset that names no read-only
 * mode can't (the hub says why); one that does is usable as any runtime is, and `note` names
 * the mode its read-only rests on.
 */
export function reviewerUsable(r: RuntimeInfo | undefined): {
  usable: boolean;
  why?: string;
  note?: string;
} {
  if (r?.acp && r.cannot_review) return { usable: false, why: r.cannot_review };
  const u = runtimeUsable(r);
  if (!r?.acp || !u.usable) return u;
  return { usable: true, note: `acp, ${r.review_mode || "read-only"} mode` };
}

/** The runtimes a role's picker lists: the implementer's never holds an ACP preset. */
export function runtimesFor(
  role: "implementer" | "reviewer",
  runtimes: RuntimeInfo[],
): RuntimeInfo[] {
  return role === "reviewer" ? runtimes : runtimes.filter((r) => !r.acp);
}

/**
 * The editor's saving request: `loop.setup.update` at the revision shown, for a setup card;
 * `loop.settings.set` for saved settings, for the project when `Save for: this project`.
 */
export function editorRequest(
  source: EditorSource,
  state: EditorState,
): { method: string; params: Json } {
  if (source.kind === "card")
    return {
      method: "loop.setup.update",
      params: {
        attn_id: source.attnId,
        revision: state.revision,
        settings: state.settings,
        save_for: state.saveFor,
      },
    };
  const project =
    state.saveFor === "project" ? state.project || source.project : undefined;
  return {
    method: "loop.settings.set",
    params: { ...(project ? { project } : {}), settings: state.settings },
  };
}
