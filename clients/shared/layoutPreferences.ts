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
