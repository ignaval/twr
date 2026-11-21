/** The composer's model and effort picker (`ModelPicker.tsx`), its logic apart from React:
 * what the trigger says, which runtimes, models and efforts it lists (from the hub's
 * catalog, `shared/models.ts`), how the keyboard moves through its columns, and which call
 * a choice makes. A worker thread changes with `thread.set_model` (another runtime is a
 * move with a handoff, confirmed first); the orchestrator with `orchestrator.use`, always
 * confirmed, since it starts a new orchestrator session. */
import {
  baseRuntime,
  effortChoices,
  modelChoices,
  type Choice,
} from "./models";
import type { Json, Thread } from "./types";

/** What the picker has chosen: a runtime, and its model and effort (`undefined`: the
 * runtime's own default). */
export interface Pick {
  runtime: string;
  model?: string;
  effort?: string;
}

/** What `thread` runs now, as a [`Pick`]. */
export function currentPick(thread: Thread): Pick {
  return {
    runtime: baseRuntime(thread.runtime),
    model: thread.model || undefined,
    effort: thread.effort || undefined,
  };
}

/** A runtime that runs its own command line (`pty`): it moves to another runtime, but
 * has no model or effort of its own. */
export function runsCommandLine(runtime?: string): boolean {
  const base = baseRuntime(runtime);
  return base === "pty" || base.startsWith("pty:");
}

/** How a reasoning effort reads in the picker: `high` → `High`, `xhigh` → `Extra high`. */
export function effortName(effort?: string): string {
  if (!effort) return "Default";
  if (effort === "xhigh") return "Extra high";
  return effort.charAt(0).toUpperCase() + effort.slice(1);
}

/** The short mark of a runtime in the trigger and the rail. */
export function runtimeMark(runtime: string): string {
  const base = baseRuntime(runtime);
  const known: Record<string, string> = {
    "claude-code": "Cl",
    codex: "Cx",
    "grok-build": "Gk",
    acp: "Ac",
    pty: ">_",
    fake: "Fk",
  };
  if (known[base]) return known[base];
  const letters = base.replace(/^acp:|^pty:/, "").replace(/[^a-z0-9]/gi, "");
  const mark = letters.slice(0, 2);
  return mark ? mark.charAt(0).toUpperCase() + mark.slice(1) : "?";
}

/** What the trigger shows for `thread`: the model it runs (as its runtime reported it, else
 * the one asked for, else `Default`), the effort, and its accessible name. */
export function triggerText(thread: Thread) {
  const model = thread.model_actual || thread.model || "Default";
  const effort = effortName(thread.effort);
  return {
    model,
    effort,
    label: `Model: ${model} · Effort: ${effort}. Change model`,
  };
}

/** The runtimes the rail lists: those the catalog lists models or efforts for, in its
 * order, with `current` (a runtime the catalog doesn't list: an ACP agent, `pty`) first. */
export function pickerRuntimes(catalog: Json, current?: string): string[] {
  const listed = Object.keys(catalog || {}).filter(
    (r) =>
      Array.isArray(catalog[r]?.models) || Array.isArray(catalog[r]?.efforts),
  );
  const base = baseRuntime(current);
  if (base && !listed.includes(base)) listed.unshift(base);
  return listed;
}

/** One row of the models column. */
export type ModelRow =
  | { kind: "default" }
  | { kind: "model"; value: string; description?: string }
  | { kind: "custom" };

/** The models column for `runtime`: its default, the catalog's models that match `query`
 * (by id or description), the model in effect when the catalog doesn't list it, and a
 * custom id. */
export function modelRows(
  catalog: Json,
  runtime: string,
  query = "",
  inEffect?: string,
): ModelRow[] {
  const q = query.trim().toLowerCase();
  const matches = (c: Choice) =>
    !q ||
    c.value.toLowerCase().includes(q) ||
    !!c.description?.toLowerCase().includes(q);
  const choices = modelChoices(catalog, runtime);
  const rows: ModelRow[] = [];
  if (!q || "default".includes(q)) rows.push({ kind: "default" });
  for (const c of choices)
    if (matches(c))
      rows.push({
        kind: "model",
        value: c.value,
        description: c.description,
      });
  if (
    inEffect &&
    !choices.some((c) => c.value === inEffect) &&
    matches({ value: inEffect })
  )
    rows.push({ kind: "model", value: inEffect, description: "set now" });
  rows.push({ kind: "custom" });
  return rows;
}

/** The efforts column for `runtime`: its default, then each effort the catalog lists. */
export function effortRows(catalog: Json, runtime: string): (string | null)[] {
  return [null, ...effortChoices(catalog, runtime).map((c) => c.value)];
}

/** Where the keyboard is in the popup. */
export type Column = "rail" | "models" | "efforts";

export interface Nav {
  column: Column;
  /** The highlighted row of each column. */
  rail: number;
  model: number;
  effort: number;
}

/** What a key in the popup does besides moving ([`navigate`]). */
export type NavEffect =
  "pick-runtime" | "pick-model" | "pick-effort" | "close" | undefined;

/**
 * A key pressed in the popup, whose focus stays in the search field (the highlighted row
 * is its `aria-activedescendant`): ↑/↓ move in the column, Enter picks the row (a model
 * moves on to the efforts), → goes to the next column and ← to the previous one (from the
 * models only while the search is empty, else it moves the caret; → from the rail only to a
 * column with models), Esc closes. Without a search (a runtime that runs its own command
 * line), the focus is on the highlighted row instead. Returns
 * the new position and what to do, or `undefined` for a key it leaves alone.
 */
export function navigate(
  nav: Nav,
  key: string,
  sizes: { rail: number; models: number; efforts: number },
  searchEmpty: boolean,
  caretAtEnd = true,
): { nav: Nav; effect: NavEffect } | undefined {
  const size = (c: Column) =>
    c === "rail" ? sizes.rail : c === "models" ? sizes.models : sizes.efforts;
  const at = (c: Column) =>
    c === "rail" ? nav.rail : c === "models" ? nav.model : nav.effort;
  const set = (c: Column, i: number): Nav =>
    c === "rail"
      ? { ...nav, rail: i }
      : c === "models"
        ? { ...nav, model: i }
        : { ...nav, effort: i };
  const c = nav.column;
  switch (key) {
    case "ArrowDown":
    case "ArrowUp": {
      const n = size(c);
      if (!n) return { nav, effect: undefined };
      const step = key === "ArrowDown" ? 1 : n - 1;
      return { nav: set(c, (at(c) + step) % n), effect: undefined };
    }
    case "ArrowRight":
      // (No models to go to: a runtime that runs its own command line stays in the rail.)
      if (c === "rail")
        return {
          nav: sizes.models > 0 ? { ...nav, column: "models" } : nav,
          effect: undefined,
        };
      if (c === "models" && caretAtEnd && sizes.efforts > 0)
        return { nav: { ...nav, column: "efforts" }, effect: undefined };
      return c === "models" ? undefined : { nav, effect: undefined };
    case "ArrowLeft":
      if (c === "efforts")
        return { nav: { ...nav, column: "models" }, effect: undefined };
      if (c === "models" && searchEmpty && sizes.rail > 1)
        return { nav: { ...nav, column: "rail" }, effect: undefined };
      return c === "models" ? undefined : { nav, effect: undefined };
    case "Enter":
      return {
        nav,
        effect:
          c === "rail"
            ? "pick-runtime"
            : c === "models"
              ? "pick-model"
              : "pick-effort",
      };
    case "Escape":
      return { nav, effect: "close" };
    default:
      return undefined;
  }
}

/** The call a choice makes, or none when nothing changed. */
export type Plan =
  | { kind: "none" }
  /** `thread.set_model`; `move`: to another runtime, with a handoff (confirmed first). */
  | { kind: "thread"; params: Json; move: boolean }
  /** `orchestrator.use` (confirmed first: a new orchestrator session). */
  | { kind: "orchestrator"; params: Json };

/**
 * What choosing `pick` for `thread` asks of the hub. A worker thread on the same runtime
 * sends only what changed (`default` for a setting cleared), so keeping one never asks the
 * hub to clear it; on another runtime it is a move, with the model and effort picked (one
 * left out is that runtime's default). The orchestrator is switched with `orchestrator.use`
 * and the handoff summary: its runtime (with its account, on the same runtime), and the model
 * and effort picked (one left out is the runtime's default).
 *
 * `account`: the orchestrator's account as `orchestrator.show` names it ([`orchAccount`]).
 * A thread records no account while it runs on the native login, and a bare runtime would
 * get the policy's default account, which may be another one: the native login goes by
 * its current name (`codex:default`), as the TUI's `/orch` sends it.
 */
export function planChange(
  target: "orchestrator" | "thread",
  thread: Thread,
  pick: Pick,
  account?: string,
): Plan {
  const now = currentPick(thread);
  const same = pick.runtime === now.runtime;
  if (target === "orchestrator") {
    if (same && pick.model === now.model && pick.effort === now.effort)
      return { kind: "none" };
    const kept = thread.account || account;
    const params: Json = {
      runtime: same && kept ? `${pick.runtime}:${kept}` : pick.runtime,
      summary: true,
    };
    if (pick.model) params.model = pick.model;
    if (pick.effort) params.effort = pick.effort;
    return { kind: "orchestrator", params };
  }
  if (!same) {
    const params: Json = { thread: thread.id, runtime: pick.runtime };
    if (pick.model) params.model = pick.model;
    if (pick.effort) params.effort = pick.effort;
    return { kind: "thread", params, move: true };
  }
  const params: Json = { thread: thread.id };
  if (pick.model !== now.model) params.model = pick.model ?? "default";
  if (pick.effort !== now.effort) params.effort = pick.effort ?? "default";
  if (!("model" in params) && !("effort" in params)) return { kind: "none" };
  return { kind: "thread", params, move: false };
}

/** The account the orchestrator `thread` runs on, from `orchestrator.show` (`show`): the
 * native login under its current name too. `undefined` when `show` is about another
 * orchestrator (it was switched meanwhile) or an older hub's, which names none. */
export function orchAccount(show: Json, thread: Thread): string | undefined {
  if (!show || show.thread !== thread.id) return undefined;
  return typeof show.account === "string" && show.account
    ? show.account
    : undefined;
}

/** Whether `orchestrator.show` (`show`) says `runtime`'s program is missing on the hub's
 * machine (`installed`): an orchestrator switched to it can't start until it is there. */
export function notInstalled(show: Json, runtime: string): boolean {
  return show?.installed?.[runtime] === false;
}

/** What went wrong with a change that went through, as its result says (`orchestrator.use`,
 * `thread.set_model`): the hub's `warning` (a runtime whose program is missing), and the new
 * orchestrator's `start_error`. `undefined`: nothing. */
export function changeProblem(result: Json): string | undefined {
  const said: string[] = [];
  if (typeof result?.warning === "string" && result.warning)
    said.push(result.warning);
  if (typeof result?.start_error === "string" && result.start_error)
    said.push(`The new orchestrator could not start: ${result.start_error}`);
  // (Each a sentence: the hub's end without a stop.)
  const sentence = (t: string) => (/[.!?]$/.test(t) ? t : t + ".");
  return said.length ? said.map(sentence).join(" ") : undefined;
}

/** Whether `event` opens the picker: Ctrl+Shift+M, ⌘+Shift+M on a Mac. */
export function opensPicker(
  event: {
    key: string;
    shiftKey: boolean;
    ctrlKey: boolean;
    metaKey: boolean;
    altKey: boolean;
  },
  mac: boolean,
): boolean {
  const mod = mac ? event.metaKey && !event.ctrlKey : event.ctrlKey;
  return (
    mod &&
    event.shiftKey &&
    !event.altKey &&
    (event.key || "").toLowerCase() === "m"
  );
}

const messages = (n: number) =>
  n === 1 ? "1 queued message" : `${n} queued messages`;

/** The orchestrator switch's confirmation text. `queued`: the messages waiting in the
 * orchestrator's queue, as the conversation shows them; the switch drops them (without the
 * summary, all of them; with it, those the orchestrator has not started when it is asked). */
export function switchDetail(queued: number): string {
  let detail =
    "This starts a new orchestrator session that begins with a handoff: what the log " +
    "shows, and with the summary, the current orchestrator's own (it is asked first, " +
    "and waited for up to 60 seconds while it is busy). Workers are not touched.";
  if (queued > 0)
    detail +=
      ` ${messages(queued)} to the orchestrator ${queued === 1 ? "is" : "are"} dropped: ` +
      "without the summary, at once; with it, any it hasn't started by the time it is " +
      "asked. You can put yours back in the message box afterwards.";
  return detail;
}

/** What an orchestrator switch (`orchestrator.use`'s result) dropped: how many queued
 * messages (`previous.dropped_prompts`), and the text of those the user sent, whole
 * (`previous.dropped_messages`), to put back in the message box. */
export function droppedBySwitch(result: Json): {
  count: number;
  mine: string[];
} {
  const previous = result?.previous || {};
  const count = Number(previous.dropped_prompts) || 0;
  const mine = Array.isArray(previous.dropped_messages)
    ? previous.dropped_messages.filter(
        (m: unknown): m is string => typeof m === "string" && m.trim() !== "",
      )
    : [];
  return { count, mine };
}

/** What the app says about a switch that dropped `count` queued messages, `mine` of them
 * the user's own: none, a notice, or an offer to put the user's back in the message box. */
export function droppedWords(
  count: number,
  mine: number,
): { notice: string } | { title: string; detail: string } | undefined {
  if (count <= 0) return undefined;
  const dropped = `The switch dropped ${messages(count)} the previous orchestrator hadn't started.`;
  if (mine <= 0) return { notice: dropped };
  return {
    title:
      mine === 1
        ? "Put your queued message back in the message box?"
        : `Put your ${mine} queued messages back in the message box?`,
    detail:
      `${dropped} ${mine === count ? (mine === 1 ? "It was yours" : "They were yours") : `${mine} of them were yours`}` +
      ": put back, with their files, you can send them to the new orchestrator.",
  };
}

/** A switch's result as the app shows it: the dropped messages' text left out (the app
 * offers them back instead), so no file's `Uploaded file:` lines show. */
export function withoutDroppedText(result: Json): Json {
  if (!result?.previous || !("dropped_messages" in result.previous))
    return result;
  const { dropped_messages: _, ...previous } = result.previous;
  return { ...result, previous };
}
