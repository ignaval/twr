import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  omarchyColorsFile,
  readOmarchyTheme,
  watchOmarchyTheme,
} from "../desktop/omarchy-theme";
import { parseOmarchyColors } from "../shared/omarchy";
import { STOCK_THEMES } from "./omarchy-themes";

let home: string;
beforeEach(() => {
  home = mkdtempSync(join(tmpdir(), "twr-omarchy-home-"));
});
afterEach(() => {
  rmSync(home, { recursive: true, force: true });
  vi.useRealTimers();
});
const put = (relative: string, text: string) => {
  const path = join(home, relative);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, text);
};
// Where omarchy-theme-set (v4.5.0) leaves the selected theme, and where Omarchy 3 did.
const STATE = ".local/state/omarchy/current/theme/colors.toml";
const LEGACY = ".config/omarchy/current/theme/colors.toml";

describe("the selected Omarchy theme", () => {
  it("is read where Omarchy 4 keeps it, for every stock theme", () => {
    for (const [name, text] of Object.entries(STOCK_THEMES)) {
      put(STATE, text);
      expect(readOmarchyTheme(home), name).toEqual(parseOmarchyColors(text));
    }
    expect(omarchyColorsFile(home)).toBe(join(home, STATE));
  });

  it("is read where Omarchy 3 kept it when Omarchy 4's folder is absent", () => {
    put(LEGACY, STOCK_THEMES["tokyo-night"]);
    expect(readOmarchyTheme(home)).toEqual(
      parseOmarchyColors(STOCK_THEMES["tokyo-night"]),
    );
    expect(omarchyColorsFile(home)).toBe(join(home, LEGACY));
  });

  it("prefers Omarchy 4's place, and never falls back to a stale old one mid-switch", () => {
    put(LEGACY, STOCK_THEMES.white);
    put(STATE, STOCK_THEMES.nord);
    expect(readOmarchyTheme(home)?.background).toBe(
      parseOmarchyColors(STOCK_THEMES.nord)?.background,
    );
    // omarchy-theme-set removes current/theme before it moves next-theme into its place.
    rmSync(dirname(join(home, STATE)), { recursive: true });
    expect(readOmarchyTheme(home)).toBeNull();
  });

  it("is null without a theme or with an unusable one", () => {
    expect(readOmarchyTheme(home)).toBeNull();
    put(STATE, 'background = "blue"\nforeground = "#ffffff"\n');
    expect(readOmarchyTheme(home)).toBeNull();
  });
});

describe("watching the Omarchy theme", () => {
  const dark = { background: "#000000", foreground: "#ffffff" };
  const light = { background: "#ffffff", foreground: "#000000" };

  it("reports a switch once, and not the moment the theme folder is missing", () => {
    vi.useFakeTimers();
    const reads: (Record<string, string> | null)[] = [dark, null, null, light];
    let current: Record<string, string> | null = dark;
    const changed = vi.fn();
    const stop = watchOmarchyTheme(
      () => (reads.length ? (current = reads.shift()!) : current),
      changed,
      { interval: 5000, settle: 250, retries: 8 },
    );
    vi.advanceTimersByTime(5000); // dark → missing
    vi.advanceTimersByTime(250); // still missing
    vi.advanceTimersByTime(250); // light
    expect(changed.mock.calls).toEqual([[light]]);
    vi.advanceTimersByTime(20000);
    expect(changed).toHaveBeenCalledTimes(1);
    stop();
  });

  it("lets go of a theme that stays gone", () => {
    vi.useFakeTimers();
    let theme: Record<string, string> | null = dark;
    const changed = vi.fn();
    const stop = watchOmarchyTheme(() => theme, changed, {
      interval: 5000,
      settle: 250,
      retries: 8,
    });
    theme = null;
    vi.advanceTimersByTime(5000 + 7 * 250);
    expect(changed).not.toHaveBeenCalled();
    vi.advanceTimersByTime(250);
    expect(changed.mock.calls).toEqual([[null]]);
    theme = light;
    vi.advanceTimersByTime(5000);
    expect(changed.mock.calls).toEqual([[null], [light]]);
    stop();
    theme = dark;
    vi.advanceTimersByTime(20000);
    expect(changed).toHaveBeenCalledTimes(2);
  });
});
