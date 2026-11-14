/* The selected Omarchy theme on this machine, and its changes (Linux).
 *
 * Omarchy keeps the selected theme's files in `~/.local/state/omarchy/current/theme` since
 * version 4 (its migration 1781043107 moved them there and removed the old place,
 * `~/.config/omarchy/current/theme`, which older releases use). Both are under $HOME,
 * whatever the XDG variables say, as Omarchy writes them. `omarchy-theme-set` switches by
 * deleting the current theme's folder and moving the next one into its place, so for a moment
 * there is no theme: a read that finds none right after one that found a theme is tried again
 * before the app lets go of the theme. */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { parseOmarchyColors } from "../shared/omarchy";

/** The `colors.toml` of the selected theme under `home`: Omarchy 4's place when Omarchy 4's
 * state folder exists (even while a switch has removed its theme), else the older place. */
export function omarchyColorsFile(home: string, exists = existsSync) {
  const state = join(home, ".local/state/omarchy/current");
  return join(
    exists(state) ? state : join(home, ".config/omarchy/current"),
    "theme/colors.toml",
  );
}

/** The selected theme's colours (`parseOmarchyColors`), or null without a usable theme. */
export function readOmarchyTheme(home: string): Record<string, string> | null {
  try {
    return parseOmarchyColors(readFileSync(omarchyColorsFile(home), "utf8"));
  } catch {
    return null;
  }
}

/** Calls `changed` with the theme each time it changes, checking every `interval` ms. A
 * theme that disappears is read again every `settle` ms, `retries` times, before `changed`
 * hears null, so a switch in progress never shows as "no theme". Returns a stop function. */
export function watchOmarchyTheme(
  read: () => Record<string, string> | null,
  changed: (theme: Record<string, string> | null) => void,
  {
    interval = 5000,
    settle = 250,
    retries = 8,
  }: { interval?: number; settle?: number; retries?: number } = {},
) {
  let previous = JSON.stringify(read());
  let missing = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopped = false;
  const check = () => {
    if (stopped) return;
    const theme = read();
    const next = JSON.stringify(theme);
    if (theme === null && previous !== "null" && missing < retries) {
      missing++;
      schedule(settle);
      return;
    }
    missing = 0;
    if (next !== previous) {
      previous = next;
      changed(theme);
    }
    schedule(interval);
  };
  const schedule = (delay: number) => {
    timer = setTimeout(check, delay);
    (timer as { unref?: () => void }).unref?.();
  };
  schedule(interval);
  return () => {
    stopped = true;
    if (timer !== undefined) clearTimeout(timer);
  };
}
