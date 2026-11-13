/* Review loops in the apps (docs/design/loops.md › Desktop and mobile): the pure logic
 * behind the inspector's Loops section, the loop chip, the Review loop offer, the setup card
 * and the settings editor. Nothing here decides what the hub decides: who may start a loop,
 * on which settings, whether settings hold, the loop line. The hub says; this reads it. */
import type { Json, Thread } from "./types";

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
/** What the hub found of one runtime on its own machine (`loop.settings.get` `runtimes`). */
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
}
/** `loop.settings.get {project?}`. */
export interface SettingsGet {
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
  [key: string]: Json;
}
/** The last loop that ended on a thread (`thread.last_loop`). */
export interface LastLoop {
  id: string;
  kind: string;
  outcome: string;
  ended_turn?: number | null;
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
 * A stopped thread is offered one too: the hub's shutdown stops every thread, which then
 * goes on (and a loop with it) as soon as it is sent something, and the thread's view does
 * not tell that stop from one of the user's. The hub refuses a thread someone stopped, with
 * the reason (`send it a message to start it again first`).
 */
export function offersReview(
  thread: Thread | undefined,
  offerReview: boolean | undefined,
): boolean {
  if (!thread || offerReview === false) return false;
  if (thread.is_orchestrator || thread.parked) return false;
  if (["parked", "attached"].includes(thread.status)) return false;
  if (thread.loop) return false;
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

/** `config.show`'s `loops.offer_review`: true unless the hub says false. */
export function offerReviewSetting(config: Json): boolean {
  return config?.loops?.offer_review !== false;
}

// ---------------------------------------------------------------------------------------
// The setup card
// ---------------------------------------------------------------------------------------

/** The setup card's options, as the hub raises them. */
export const SETUP_START = "start and save these settings";
export const SETUP_CANCEL = "cancel";

export interface SetupSummary {
  /** The first line: what the card asks. */
  title: string;
  /** `Label   value` rows, in order. */
  rows: [string, string][];
  warnings: string[];
  /** What has to change before the loop can start (`Change first: …`). */
  problems: string[];
  /** Lines that are none of these, kept as they are. */
  other: string[];
}

/**
 * Reads a setup card's summary: a sentence, then `Label   value` rows (a label, then two or
 * more spaces), then `Warning: …` and `Change first: …` lines. A line it can't place stays in
 * `other`, so nothing the hub wrote is lost.
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
  for (const line of lines) {
    if (!line.trim()) continue;
    if (first) {
      out.title = line.trim();
      first = false;
      continue;
    }
    const warning = /^\s*Warning:\s*(.*)$/i.exec(line);
    const problem = /^\s*Change first:\s*(.*)$/i.exec(line);
    const row = /^\s*([A-Za-z][A-Za-z ]{0,30}?)\s{2,}(\S.*)$/.exec(line);
    if (warning) out.warnings.push(warning[1].trim());
    else if (problem) out.problems.push(problem[1].trim());
    else if (row) out.rows.push([row[1].trim(), row[2].trim()]);
    else out.other.push(line.trim());
  }
  return out;
}

/** Whether `item` is a loop's setup card, and whether Start and save may be pressed. */
export function setupCardState(item: {
  loop_setup?: boolean;
  summary: string;
  options?: string[];
}) {
  const parsed = parseSetupSummary(item.summary);
  const options = item.options || [];
  return {
    parsed,
    canStart: options.includes(SETUP_START) && parsed.problems.length === 0,
    canCancel: options.includes(SETUP_CANCEL),
  };
}

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

/**
 * The phases running loop `state` passed over without running them: the verification when
 * the ladder's end skipped it (`transition.verify_skipped`: the last tier's round found
 * nothing at the verification's effort, so v1 could only repeat it), and the sweep once the
 * ladder ended without one (the profile has none, the ladder fixed nothing, or the loop
 * skipped to verify).
 */
export function skippedPhases(state: LoopState): string[] {
  const transition = state.transition as
    { reason?: string; verify_skipped?: boolean } | null | undefined;
  if (!transition) return [];
  const out: string[] = [];
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
  l: Pick<LoopState, "waiting_on" | "paused" | "stuck">,
) {
  if (l.stuck) return `stuck: ${l.stuck}`;
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

/**
 * The loops that ended since `known` (thread id → the id of its last loop, as last seen), as
 * the words a screen reader hears (`api's review loop ended: clean`), and the new `known`.
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
    said.push(`${t.name}'s ${kind} loop ended${how ? `: ${how}` : ""}`);
  }
  return { said, known: next };
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
