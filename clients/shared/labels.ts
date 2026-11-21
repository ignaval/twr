/** Plain-language labels for values the hub stores as ids. Every picker that shows one of
 * these values (Settings, command forms, the thread inspector) takes its words from here,
 * so the same mode reads the same everywhere. The value saved is always the id. */

export interface Choice {
  value: string;
  label: string;
  description?: string;
}

/** twr's own permission modes, narrowest first (docs/runtimes.md#permission-modes). */
export const PERMISSION_MODES: Choice[] = [
  {
    value: "plan",
    label: "Plan only",
    description: "Plans and reads; changes nothing.",
  },
  {
    value: "read-only",
    label: "Read only",
    description: "Reads files; asks before anything else.",
  },
  {
    value: "dontAsk",
    label: "Read only, never ask",
    description: "Denies whatever isn't allowed instead of asking.",
  },
  {
    value: "manual",
    label: "Ask before edits and commands",
    description: "Every edit and command waits for you.",
  },
  {
    value: "acceptEdits",
    label: "Accept edits, ask for commands",
    description: "File edits go ahead; commands wait for you.",
  },
  {
    value: "auto",
    label: "Agent decides",
    description: "The agent's own auto mode decides what to ask (the default).",
  },
  {
    value: "bypassPermissions",
    label: "Ask nothing",
    description: "Runs every edit and command without asking.",
  },
];

/** The modes a device without `admin` may choose (the hub refuses wider ones). */
export const NARROW_PERMISSION_MODES = [
  "manual",
  "plan",
  "read-only",
  "dontAsk",
];

/** A mode's label, or the id itself for a runtime's own mode twr has no words for. */
export function permissionLabel(mode?: string | null): string {
  if (!mode) return "";
  return PERMISSION_MODES.find((m) => m.value === mode)?.label ?? mode;
}

/** The choices for a permission-mode picker: twr's modes (narrowed without `admin`),
 * plus the current value when it is a runtime's own mode, so it still shows. */
export function permissionChoices(
  current?: string | null,
  admin = true,
): Choice[] {
  const modes = admin
    ? PERMISSION_MODES
    : PERMISSION_MODES.filter((m) => NARROW_PERMISSION_MODES.includes(m.value));
  return current && !modes.some((m) => m.value === current)
    ? [...modes, { value: current, label: current }]
    : modes;
}

/** A duration in seconds, in words: 0 is "Off", whole hours and minutes read as such. */
export function durationLabel(secs: number): string {
  if (secs === 0) return "Off";
  if (secs % 86400 === 0) return plural(secs / 86400, "day");
  if (secs % 3600 === 0) return plural(secs / 3600, "hour");
  if (secs % 60 === 0) return plural(secs / 60, "minute");
  return plural(secs, "second");
}

/** Choices for a duration select: the presets, plus the current value when it isn't one,
 * so a value set in the config file still shows (and saving another value replaces it). */
export function durationChoices(
  presets: number[],
  current?: number | null,
  unit: "seconds" | "days" = "seconds",
): Choice[] {
  const label = (n: number) =>
    unit === "days" ? (n === 0 ? "Off" : plural(n, "day")) : durationLabel(n);
  const values =
    current != null && !presets.includes(current)
      ? [...presets, current].sort((a, b) => a - b)
      : presets;
  return values.map((n) => ({ value: String(n), label: label(n) }));
}

function plural(n: number, word: string) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}
