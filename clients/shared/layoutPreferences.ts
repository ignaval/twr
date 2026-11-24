import { useSyncExternalStore } from "react";

export interface LayoutPreferences {
  sidebar: number;
  inspector: number;
  density: "compact" | "comfortable";
  textSize: number;
  codeSize: number;
}
export const defaultLayout: LayoutPreferences = {
  sidebar: 260,
  inspector: 320,
  density: "comfortable",
  textSize: 13,
  codeSize: 13,
};
const storageKey = "twr.layout.v1";
const clamp = (value: unknown, fallback: number, min: number, max: number) =>
  typeof value === "number" && Number.isFinite(value)
    ? Math.max(min, Math.min(max, Math.round(value)))
    : fallback;
export function validateLayout(value: unknown): LayoutPreferences {
  const v =
    value && typeof value === "object"
      ? (value as Partial<LayoutPreferences>)
      : {};
  return {
    sidebar: clamp(v.sidebar, 260, 200, 440),
    inspector: clamp(v.inspector, 320, 280, 600),
    density: v.density === "compact" ? "compact" : "comfortable",
    textSize: clamp(v.textSize, 13, 12, 22),
    codeSize: clamp(v.codeSize, 13, 11, 24),
  };
}
function load() {
  try {
    return validateLayout(
      JSON.parse(localStorage.getItem(storageKey) || "null"),
    );
  } catch {
    return { ...defaultLayout };
  }
}
let current = load();
let systemScale = 1;
try {
  systemScale = Math.max(
    0.8,
    Math.min(3, Number(localStorage.getItem("twr.text-scale")) || 1),
  );
} catch {}
const subscribers = new Set<() => void>();
function apply() {
  const root = document.documentElement;
  root.dataset.density = current.density;
  root.style.setProperty("--sidebar-width", `${current.sidebar}px`);
  root.style.setProperty("--inspector-width", `${current.inspector}px`);
  root.style.setProperty(
    "--text-size",
    `${Math.min(44, current.textSize * Math.min(systemScale, 2.25))}px`,
  );
  root.style.setProperty(
    "--code-size",
    `${Math.min(32, current.codeSize * Math.min(systemScale, 2))}px`,
  );
  root.style.setProperty(
    "--reading-size",
    `${current.textSize * systemScale}px`,
  );
  window.dispatchEvent(new Event("twr:typography"));
}
export function initializeLayout() {
  apply();
}
export function setSystemTextScale(scale: number) {
  systemScale = Number.isFinite(scale) ? Math.max(0.8, Math.min(3, scale)) : 1;
  try {
    localStorage.setItem("twr.text-scale", String(systemScale));
  } catch {}
  apply();
}
export function updateLayout(patch: Partial<LayoutPreferences>) {
  current = validateLayout({ ...current, ...patch });
  try {
    localStorage.setItem(storageKey, JSON.stringify(current));
  } catch {
    /* Session preferences still apply. */
  }
  apply();
  subscribers.forEach((fn) => fn());
}
export function useLayoutPreferences() {
  return useSyncExternalStore(
    (callback) => {
      subscribers.add(callback);
      return () => {
        subscribers.delete(callback);
      };
    },
    () => current,
  );
}

/** Settings › Layout & text's sizes, before system scaling: the defaults are the second,
 * the largest the most each setting allows. */
export const TEXT_SIZE_PRESETS = {
  textSize: [12, 13, 16, 22],
  codeSize: [11, 13, 16, 24],
} as const;
const SIZE_NAMES = ["Small", "Default", "Large", "Extra large"];

/** A text size select's choices: the presets in words, and a size set another way (an
 * earlier version's slider) as "Custom (N px)" so it still shows. */
export function textSizeChoices(
  kind: keyof typeof TEXT_SIZE_PRESETS,
  current: number,
): { value: string; label: string }[] {
  const presets: readonly number[] = TEXT_SIZE_PRESETS[kind];
  const choices = presets.map((size, i) => ({
    value: String(size),
    label: SIZE_NAMES[i],
  }));
  if (presets.includes(current)) return choices;
  const custom = { value: String(current), label: `Custom (${current} px)` };
  const at = presets.findIndex((size) => size > current);
  return at < 0
    ? [...choices, custom]
    : [...choices.slice(0, at), custom, ...choices.slice(at)];
}

export type TextSizeStep = "larger" | "smaller" | "reset";

/** Ctrl (⌘ on Apple) with + or = (the + key without Shift), - or 0: a step of the text
 * size shortcuts, or undefined for any other key. */
export function textSizeShortcut(
  event: Pick<KeyboardEvent, "key" | "ctrlKey" | "metaKey" | "altKey">,
  mac: boolean,
): TextSizeStep | undefined {
  if (event.altKey || !(mac ? event.metaKey : event.ctrlKey)) return;
  if (event.key === "+" || event.key === "=") return "larger";
  if (event.key === "-" || event.key === "_") return "smaller";
  if (event.key === "0") return "reset";
}

/** One step of the shortcuts: interface and code text each move a pixel within the
 * bounds validateLayout keeps, so their difference holds until one reaches its end;
 * reset returns both to the defaults. */
export function steppedLayout(
  prefs: LayoutPreferences,
  step: TextSizeStep,
): Pick<LayoutPreferences, "textSize" | "codeSize"> {
  if (step === "reset")
    return {
      textSize: defaultLayout.textSize,
      codeSize: defaultLayout.codeSize,
    };
  const by = step === "larger" ? 1 : -1;
  const next = validateLayout({
    ...prefs,
    textSize: prefs.textSize + by,
    codeSize: prefs.codeSize + by,
  });
  return { textSize: next.textSize, codeSize: next.codeSize };
}

/** On the desktop one key press can reach both the window's keydown handler and the
 * View menu's accelerator, depending on the platform; a step from the other source
 * within this window of the last one is that same press, not a second. */
const SAME_PRESS_MS = 250;
export function textSizeStepGate(now: () => number = Date.now) {
  let last: { at: number; step: TextSizeStep; source: string } | undefined;
  return (step: TextSizeStep, source: "keyboard" | "menu") => {
    const at = now();
    const duplicate =
      last !== undefined &&
      last.source !== source &&
      last.step === step &&
      at - last.at < SAME_PRESS_MS;
    // A duplicate leaves the press it repeats as the one to compare with, so a held key
    // that both sources see keeps stepping once per repeat.
    if (!duplicate) last = { at, step, source };
    return !duplicate;
  };
}
const stepGate = textSizeStepGate();

export function stepTextSize(step: TextSizeStep, source: "keyboard" | "menu") {
  if (stepGate(step, source)) updateLayout(steppedLayout(current, step));
}
