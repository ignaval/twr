/** The pure logic of Settings and the Manage pages: which tab a section opens, the
 * merged Manage pages and their parts, and the choices of the New thread defaults (the
 * values the config file takes, in words). Kept free of React so tests can check it. */
import { durationLabel, type Choice } from "./labels";
import type { Json } from "./types";

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

/** The section the navigation highlights for `section`: a part of a merged page is its
 * page (boxes → accounts, usage → jobs), and Settings' Voice and Advanced tabs are
 * Settings. */
export function navSection(section: Section): Section {
  switch (section) {
    case "boxes":
      return "accounts";
    case "usage":
      return "jobs";
    case "speech":
    case "maintenance":
      return "settings";
    default:
      return section;
  }
}

// ---------------------------------------------------------------------------------------
// Settings tabs
// ---------------------------------------------------------------------------------------

export type SettingsTab = "general" | "agents" | "loops" | "voice" | "advanced";

export const SETTINGS_TABS: { id: SettingsTab; label: string }[] = [
  { id: "general", label: "General" },
  { id: "agents", label: "Agents" },
  { id: "loops", label: "Loops" },
  { id: "voice", label: "Voice" },
  { id: "advanced", label: "Advanced" },
];

/** The tabs a device sees. Without `admin` the hub's settings are out of reach: no Agents
 * tab, and Voice only when this device has voices of its own to choose. Advanced stays for
 * this device's offline history and diagnostics. */
export function settingsTabs(admin: boolean, voicePrefs: boolean) {
  return SETTINGS_TABS.filter(
    (t) => admin || (t.id !== "agents" && (t.id !== "voice" || voicePrefs)),
  );
}

/** The tab a section opens on: Read aloud is Voice, Hub & storage is Advanced. */
export function tabForSection(section: Section): SettingsTab | undefined {
  if (section === "speech") return "voice";
  if (section === "maintenance") return "advanced";
  return undefined;
}

/** The tab Settings opens on: the section's own, else the one used last on this device,
 * else General. A tab this device can't see falls back to General. */
export function initialSettingsTab(
  section: Section,
  saved: string | null | undefined,
  tabs: { id: SettingsTab }[],
): SettingsTab {
  const wanted = tabForSection(section) ?? saved;
  return tabs.some((t) => t.id === wanted)
    ? (wanted as SettingsTab)
    : "general";
}

const TAB_KEY = "twr.settings.tab";

/** The tab used last on this device (none when storage is unavailable). */
export function savedSettingsTab(): string | null {
  try {
    return localStorage.getItem(TAB_KEY);
  } catch {
    return null;
  }
}

export function saveSettingsTab(tab: SettingsTab) {
  try {
    localStorage.setItem(TAB_KEY, tab);
  } catch {
    /* Remembered for this session only. */
  }
}

// ---------------------------------------------------------------------------------------
// Manage pages
// ---------------------------------------------------------------------------------------

export type ManagePart =
  "accounts" | "boxes" | "jobs" | "usage" | "memory" | "trash";

export interface ManagePage {
  /** The section the page is listed under. */
  id: Section;
  title: string;
  description: string;
  parts: ManagePart[];
}

const PAGES: ManagePage[] = [
  {
    id: "accounts",
    title: "Machines & accounts",
    description: "Your subscriptions, and the machines where your agents work.",
    parts: ["accounts", "boxes"],
  },
  {
    id: "jobs",
    title: "Activity",
    description: "Commands and background work, and your subscription limits.",
    parts: ["jobs", "usage"],
  },
  {
    id: "memory",
    title: "Context & memory",
    description: "What your orchestrator knows between conversations.",
    parts: ["memory"],
  },
  {
    id: "trash",
    title: "Trash",
    description:
      "Deleted threads stay recoverable until their retention period ends.",
    parts: ["trash"],
  },
];

/** The Manage page a section shows (none for Settings and the other views). */
export function managePage(section: Section): ManagePage | undefined {
  const id = navSection(section);
  return PAGES.find((p) => p.id === id);
}

/** The part a section scrolls to on its page: Boxes and Usage are a page's second part. */
export function focusPart(section: Section): ManagePart | undefined {
  return section === "boxes" || section === "usage" ? section : undefined;
}

export const PART_METHOD: Record<ManagePart, string> = {
  accounts: "account.list",
  boxes: "hub.info",
  jobs: "job.list",
  usage: "usage.get",
  memory: "memory.list",
  trash: "trash.list",
};

/** The parts read again every few seconds while they are on screen. */
export const LIVE_PARTS: ManagePart[] = ["jobs", "boxes", "usage", "trash"];

// ---------------------------------------------------------------------------------------
// New thread defaults: the values the config file takes, in words
// ---------------------------------------------------------------------------------------

export const COMPACTION_CHOICES: Choice[] = [
  { value: "after_expiry", label: "After the cache expires" },
  { value: "before_expiry", label: "Also just before it expires" },
  { value: "off", label: "Off" },
];

export const CARD_ORDER_CHOICES: Choice[] = [
  { value: "stable", label: "Keep their place" },
  { value: "attention", label: "Needs attention first" },
];

export const CODEX_AUTO_CHOICES: Choice[] = [
  { value: "auto_review", label: "Codex's reviewer decides" },
  { value: "ask", label: "Ask me" },
  { value: "never", label: "Never run it" },
];

/** Seconds. */
export const STALL_PRESETS = [0, 300, 600, 1800, 3600];
export const BACKGROUND_TIMEOUT_PRESETS = [
  0, 3600, 14400, 43200, 86400, 259200,
];
export const WAKE_AFTER_PRESETS = [0, 30, 60, 300, 900];
/** Days. Zero (purge at the next cleanup) is not offered: the config file can still say so. */
export const TRASH_DAYS_PRESETS = [1, 7, 14, 30];
/** Days; the hub keeps at least 7. */
export const LOG_DAYS_PRESETS = [7, 30, 90, 365];

/** `[tui] summary_every` choices, as the config spells them. */
export const SUMMARY_EVERY_PRESETS = ["1m", "5m", "15m", "30m"];

/** The summary interval's choices, and the one selected: `span` (the config's spelling)
 * when it is a preset, else the preset of the same length (`secs`), else the current value
 * as its own choice. */
export function summaryEveryChoices(
  span?: string | null,
  secs?: number | null,
): { value: string; choices: Choice[] } {
  const choices = SUMMARY_EVERY_PRESETS.map((value) => ({
    value,
    label: durationLabel(Number(value.slice(0, -1)) * 60),
  }));
  if (span && SUMMARY_EVERY_PRESETS.includes(span))
    return { value: span, choices };
  const same = secs
    ? SUMMARY_EVERY_PRESETS[
        SUMMARY_EVERY_PRESETS.findIndex(
          (p) => Number(p.slice(0, -1)) * 60 === secs,
        )
      ]
    : undefined;
  if (same) return { value: same, choices };
  if (!span && !secs) return { value: "5m", choices };
  const value = span || `${secs}s`;
  return {
    value,
    choices: [...choices, { value, label: secs ? durationLabel(secs) : value }],
  };
}

/** Choices from a fixed list, plus the current value when the list lacks it (a value the
 * config file names that this version has no words for). */
export function withCurrent(choices: Choice[], current?: string | null) {
  return current && !choices.some((c) => c.value === current)
    ? [...choices, { value: current, label: current }]
    : choices;
}

/** The runtimes a default runtime can be: the agents the hub found (`hub.info` `agents`)
 * and the runtimes the orchestrator can run on (`orchestrator.show`, those it says are
 * installed), plus the current value. Empty when the hub lists none (an older hub): the
 * picker is free text then. */
export function runtimeChoices(
  info: Json | undefined,
  show: Json | undefined,
  current?: string | null,
): string[] {
  const found: string[] = [];
  for (const a of Array.isArray(info?.agents) ? info.agents : [])
    if (typeof a?.runtime === "string" && a.found !== false)
      found.push(a.runtime);
  for (const r of Array.isArray(show?.runtimes) ? show.runtimes : [])
    if (typeof r === "string" && show?.installed?.[r] !== false) found.push(r);
  const list = [...new Set(found)];
  if (!list.length) return [];
  return current && !list.includes(current) ? [...list, current] : list;
}

/** Whether a runtime is installed where the hub runs (Codex's own settings show only
 * then). */
export function runtimeInstalled(
  runtime: string,
  info: Json | undefined,
  show: Json | undefined,
) {
  if (show?.installed && runtime in show.installed)
    return show.installed[runtime] === true;
  return (Array.isArray(info?.agents) ? info.agents : []).some(
    (a: Json) => a?.runtime === runtime && a.found !== false,
  );
}

/** Settings the hub reads when it starts: a change applies after it restarts. The `[tui]`
 * ones are this app's, read again at once. */
export function hubApplied(table: string[]) {
  return table[0] !== "tui";
}

/** Words per minute a speaker can read at, with the current rate when it isn't one. */
export function speechRateChoices(current?: number | null): Choice[] {
  const presets = [150, 175, 200, 225, 250, 300];
  const values =
    current && !presets.includes(current)
      ? [...presets, current].sort((a, b) => a - b)
      : presets;
  return values.map((n) => ({
    value: String(n),
    label: `${n} words a minute`,
  }));
}

/** What became of one value given to a `latestWrites` writer: written (it was the latest
 * asked for), failed (the latest, with its `error`), or superseded: a newer value came
 * before it was written, or while it was, and the newer one's outcome is what counts. */
export type WriteOutcome =
  | { status: "saved" }
  | { status: "failed"; error: unknown }
  | { status: "superseded" };

/**
 * One setting's writes (a whole list each time, for `project_roots`), one at a time: a
 * value asked for while another is being written waits until that one ends, and of those
 * waiting only the latest is written, so the last change wins however the writes would have
 * raced. `idle` runs once the last write ends, saved or not (to read the file again then,
 * not between writes).
 */
export function latestWrites<T>(
  write: (value: T) => Promise<unknown>,
  idle?: () => void,
): (value: T) => Promise<WriteOutcome> {
  type Done = (outcome: WriteOutcome) => void;
  let running = false;
  let waiting: { value: T; done: Done } | undefined;
  const run = async (value: T, done: Done) => {
    running = true;
    let outcome: WriteOutcome;
    try {
      await write(value);
      outcome = { status: "saved" };
    } catch (error) {
      outcome = { status: "failed", error };
    }
    const next = waiting;
    waiting = undefined;
    if (next) {
      done({ status: "superseded" });
      void run(next.value, next.done);
      return;
    }
    running = false;
    done(outcome);
    idle?.();
  };
  return (value) =>
    new Promise<WriteOutcome>((done) => {
      if (!running) {
        void run(value, done);
        return;
      }
      waiting?.done({ status: "superseded" });
      waiting = { value, done };
    });
}
