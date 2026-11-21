// How much of each subscription window is left, as the usage limits show it
// (UsageLimits.tsx): from `usage.get` (twr-hub `usage_json`), whose windows carry the
// share used, the share left (`remaining_pct`), when they reset and their length in
// minutes. Always the share left, never the share used, as the TUI's status bar.
import type { Json } from "./types";

/** Left at or below this, a window is getting low (twr-core `usage::WARN_PCT`). */
export const WARN_PCT = 25;
/** Left at or below this, a window is low (twr-core `usage::LOW_PCT`). */
export const LOW_PCT = 10;
/** Data at least this old is shown with its age (twr-core `usage::REFRESH_AFTER`). */
export const STALE_SECS = 600;

export type Level = "normal" | "warn" | "low";

/** A window's share left, in percent, clamped to 0–100. */
export function remainingPct(window: Json): number {
  const left =
    typeof window.remaining_pct === "number"
      ? window.remaining_pct
      : 100 - Number(window.used_pct ?? 0);
  return Number.isFinite(left) ? Math.min(100, Math.max(0, left)) : 0;
}

/** How low a share left is, before rounding, as the TUI draws it. */
export function levelOf(left: number): Level {
  return left <= LOW_PCT ? "low" : left <= WARN_PCT ? "warn" : "normal";
}

/**
 * The share of the window's time still to run, in percent: where the share left would
 * be if it were spent evenly over the window. Only when the window says both its length
 * and when it resets; otherwise there is no even pace to draw.
 */
export function paceLeftPct(window: Json, now: number): number | undefined {
  const minutes = Number(window.minutes);
  const resets = Date.parse(window.resets_at ?? "");
  if (!window.minutes || !Number.isFinite(minutes) || minutes <= 0) return;
  if (!Number.isFinite(resets)) return;
  const left = (resets - now) / (minutes * 60_000);
  return Math.min(100, Math.max(0, left * 100));
}

/** A wait in its two largest units: `45m`, `2h 5m`, `3d 4h`; `under a minute` below one. */
export function waitText(ms: number): string {
  const minutes = Math.floor(Math.max(0, ms) / 60_000);
  if (minutes < 1) return "under a minute";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24)
    return minutes % 60 ? `${hours}h ${minutes % 60}m` : `${hours}h`;
  const days = Math.floor(hours / 24);
  return hours % 24 ? `${days}d ${hours % 24}h` : `${days}d`;
}

/** An age in its largest whole unit: `40s`, `12m`, `3h`, `2d` (twr-core `usage::age`). */
export function ageText(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** `resets in 2h`, or `resets now` once the time has come. */
export function resetText(resetsAt: string, now: number): string {
  const at = Date.parse(resetsAt);
  if (!Number.isFinite(at)) return "";
  return at <= now ? "resets now" : `resets in ${waitText(at - now)}`;
}

/** A reset time in local time: the time within a day, else the weekday and date too. */
export function resetTime(resetsAt: string, now: number): string {
  const at = new Date(resetsAt);
  if (!Number.isFinite(at.getTime())) return "";
  return at.getTime() - now < 86_400_000
    ? at.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })
    : at.toLocaleString(undefined, {
        weekday: "short",
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      });
}

/** A window's name: `5-hour`, `Weekly`, `Monthly`, else its length (`90m`, `8h`). */
export function windowName(window: Json): string {
  switch (window.window) {
    case "five_hour":
      return "5-hour";
    case "seven_day":
      return "Weekly";
    case "month":
      return "Monthly";
  }
  return String(window.label || window.window || "Window");
}

/** Everything a window row says, for its text, tooltip and screen readers. */
export interface WindowView {
  name: string;
  /** Share left, in percent (0–100), and rounded for text. */
  left: number;
  leftText: string;
  level: Level;
  /** Share of the window's time left, when known. */
  pace?: number;
  reset?: string;
  resetAt?: string;
  /** The tooltip. */
  title: string;
  /** What assistive technology reads for the bar. */
  valueText: string;
}

export function windowView(window: Json, now: number): WindowView {
  const left = remainingPct(window);
  const pace = paceLeftPct(window, now);
  const reset = window.resets_at ? resetText(window.resets_at, now) : undefined;
  const resetAt = window.resets_at
    ? resetTime(window.resets_at, now)
    : undefined;
  const leftText = `${Math.round(left)}% left`;
  const title = [
    leftText +
      (pace === undefined
        ? "."
        : ` · ${Math.round(pace)}% of the window left. The line is where even spending would be.`),
    resetAt ? `Resets ${resetAt}.` : "",
  ]
    .filter(Boolean)
    .join(" ");
  const valueText = [
    leftText,
    pace === undefined ? "" : `${Math.round(pace)}% of the window's time left`,
    reset || "",
  ]
    .filter(Boolean)
    .join(", ");
  return {
    name: windowName(window),
    left,
    leftText,
    level: levelOf(left),
    pace,
    reset,
    resetAt,
    title,
    valueText,
  };
}

/** One runtime's account as shown: its label (`Claude`, `Claude:work`), plan, windows and age. */
export interface AccountView {
  key: string;
  label: string;
  plan?: string;
  windows: WindowView[];
  /** The data's age in seconds, now. */
  age: number;
  stale: boolean;
}

/**
 * The hub's clock `now` (this device's clock) for a `usage.get` answer read at `fetchedAt`
 * (also this device's): the answer's own `now` plus the time since. The resets are the
 * providers' times, which the hub's clock is compared with; this device's may be off. An
 * answer without `now` (an older hub): this device's clock.
 */
export function hubNow(data: Json | undefined, now: number, fetchedAt = now) {
  const served = Date.parse(data?.now ?? "");
  return Number.isFinite(served) ? served + Math.max(0, now - fetchedAt) : now;
}

/**
 * The accounts of a `usage.get` answer, as they are `now`: the answer was read at
 * `fetchedAt`, so ages have grown since by the time between. Windows that have reset since
 * are left out, as the hub does when it answers. Resets, countdowns and pace are reckoned
 * on the hub's clock ([`hubNow`]), not this device's.
 */
export function accountsOf(
  data: Json | undefined,
  now: number,
  fetchedAt = now,
): AccountView[] {
  const runtimes: Json[] = Array.isArray(data?.runtimes) ? data.runtimes : [];
  const at = hubNow(data, now, fetchedAt);
  return runtimes
    .map((runtime) => {
      const age =
        Number(runtime.age_secs ?? 0) + Math.max(0, now - fetchedAt) / 1000;
      const windows = (Array.isArray(runtime.windows) ? runtime.windows : [])
        .filter(
          (window: Json) =>
            !window.resets_at || Date.parse(window.resets_at) > at,
        )
        .map((window: Json) => windowView(window, at));
      return {
        key: `${runtime.runtime}:${runtime.account ?? ""}`,
        label: String(runtime.label || runtime.runtime),
        plan: runtime.plan || undefined,
        windows,
        age,
        stale: Boolean(runtime.stale) || age >= STALE_SECS,
      };
    })
    .filter((account) => account.windows.length);
}
