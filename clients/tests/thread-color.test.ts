import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  MIN_CONTRAST,
  SURFACES,
  THREAD_COLORS,
  THREAD_PALETTE,
  TINTS,
  backgrounds,
  contrast,
  cubeHex,
  lowestContrast,
  over,
  parseHex,
  threadInk,
  threadStyle,
  type Surfaces,
  type Theme,
} from "../shared/threadColor";

/* A thread's name drawn in its colour reads at 4.5:1 (WCAG AA) for every one of the 30
 * colours, in both themes, on every background a name sits on: the colour is kept for the
 * thread's marks, and the name uses the colour's ink (shared/threadColor.ts). */

const root = join(import.meta.dirname, "..");
const css = readFileSync(join(root, "shared/style.css"), "utf8");
const app = readFileSync(join(root, "shared/App.tsx"), "utf8");
const themes: Theme[] = ["light", "dark"];

/** The declarations of the first rule whose selector is exactly `selector`. */
function rule(source: string, selector: string) {
  const at = source.indexOf(selector + " {");
  expect(at, selector).toBeGreaterThanOrEqual(0);
  return source.slice(at, source.indexOf("}", at));
}
/** A theme block's token `name`: `#rrggbb`, or `rgb(r g b / a%)` as a colour and alpha. */
function token(block: string, name: string): string | [number[], number] {
  const m = new RegExp(`--${name}:\\s*([^;]+);`).exec(block)?.[1].trim();
  expect(m, name).toBeTruthy();
  const rgb = /^rgb\((\d+) (\d+) (\d+) \/ ([\d.]+)%\)$/.exec(m!);
  return rgb ? [rgb.slice(1, 4).map(Number), Number(rgb[4]) / 100] : m!;
}
/** OKLab hue angle (degrees) and chroma of `#rrggbb`. */
function hue(hex: string) {
  const lin = parseHex(hex)!.map((v) => {
    const s = v / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  const l = Math.cbrt(
    0.4122214708 * lin[0] + 0.5363325363 * lin[1] + 0.0514459929 * lin[2],
  );
  const m = Math.cbrt(
    0.2119034982 * lin[0] + 0.6806995451 * lin[1] + 0.1073969566 * lin[2],
  );
  const s = Math.cbrt(
    0.0883024619 * lin[0] + 0.2817188376 * lin[1] + 0.6299787005 * lin[2],
  );
  const a = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
  const b = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
  return {
    angle: (Math.atan2(b, a) * 180) / Math.PI,
    chroma: Math.hypot(a, b),
  };
}

describe("thread colours", () => {
  it("mirrors twr_core::palette's 30 colours", () => {
    const rust = readFileSync(
      join(root, "../crates/twr-core/src/palette.rs"),
      "utf8",
    );
    const body = /THREAD_PALETTE: \[u8; 30\] = \[([^\]]+)\]/.exec(rust)![1];
    expect([...THREAD_PALETTE]).toEqual(
      body
        .split(",")
        .map((n) => Number(n.trim()))
        .filter((n) => n),
    );
    expect(/LEVELS: \[u8; 6\] = \[0, 95, 135, 175, 215, 255\]/.test(rust)).toBe(
      true,
    );
    // What twr-app sends as a thread's `accent` (`#{r:02x}{g:02x}{b:02x}`).
    expect(cubeHex(31)).toBe("#0087af");
    expect(cubeHex(201)).toBe("#ff00ff");
    expect(new Set(THREAD_COLORS).size).toBe(30);
  });

  it("knows the backgrounds style.css gives the themes", () => {
    const dark = css.slice(0, css.indexOf("}"));
    const lightAt = css.indexOf(':root[data-theme="light"] {');
    const light = css.slice(lightAt, css.indexOf("}", lightAt));
    for (const [theme, block] of [
      ["dark", dark],
      ["light", light],
    ] as const) {
      const s = SURFACES[theme];
      for (const name of ["bg", "panel", "raised", "overlay"] as const)
        expect(token(block, name), `${theme} ${name}`).toBe(s[name]);
      // hover and active are white or black at an opacity.
      const [hoverRgb, hoverAlpha] = token(block, "hover") as [
        number[],
        number,
      ];
      expect([hoverRgb, hoverAlpha]).toEqual([s.hover[0], s.hover[1]]);
      const [activeRgb, activeAlpha] = token(block, "active") as [
        number[],
        number,
      ];
      expect([activeRgb, activeAlpha]).toEqual([s.active[0], s.active[1]]);
    }
  });

  it("knows the tints a name sits on", () => {
    // A dived card, the "To:" chip, the conversation's symbol.
    expect(rule(css, ".thread-card.dived")).toContain(
      "color-mix(in srgb, var(--thread-color) 8%, var(--panel))",
    );
    expect(rule(css, ".destination-chip")).toContain(
      "color-mix(in srgb, var(--destination) 12%, transparent)",
    );
    expect(rule(css, ".destination-symbol.worker")).toContain(
      "color-mix(in srgb, var(--destination) 12%, transparent)",
    );
    expect(TINTS).toEqual([0.08, 0.12]);
  });

  it("reads at 4.5:1 for all 30 colours, both themes, every background a name is on", () => {
    const failures: string[] = [];
    let checked = 0;
    for (const theme of themes)
      for (const color of THREAD_COLORS) {
        const ink = parseHex(threadInk(color, theme))!;
        for (const [on, bg] of backgrounds(theme, parseHex(color)!)) {
          checked++;
          const ratio = contrast(ink, bg);
          if (ratio < MIN_CONTRAST)
            failures.push(
              `${color} (${theme}) on ${on}: ${ratio.toFixed(2)}:1`,
            );
        }
      }
    // 30 colours × 2 themes × (4 surfaces × (plain, hover, selected, 2 tints × (plain, hover))).
    expect(checked).toBe(30 * 2 * 4 * 7);
    expect(failures).toEqual([]);
  });

  it("keeps a colour that already reads, and changes the others only as far as needed", () => {
    for (const theme of themes)
      for (const color of THREAD_COLORS) {
        const ink = threadInk(color, theme);
        const own = lowestContrast(color, color, theme).ratio;
        if (own >= 4.6) expect(ink, `${color} ${theme}`).toBe(color);
        else {
          expect(ink).not.toBe(color);
          // Darker in the light theme, lighter in the dark one.
          const [a, b] = [parseHex(ink)!, parseHex(color)!];
          const lum = (c: number[]) => contrast(c as never, [0, 0, 0]);
          if (theme === "light") expect(lum(a)).toBeLessThan(lum(b));
          else expect(lum(a)).toBeGreaterThan(lum(b));
          // Not much past the target.
          expect(lowestContrast(ink, color, theme).ratio).toBeLessThan(5);
        }
      }
  });

  it("keeps each colour's hue, so a name still looks like its thread", () => {
    for (const theme of themes)
      for (const color of THREAD_COLORS) {
        const before = hue(color);
        const after = hue(threadInk(color, theme));
        if (before.chroma < 0.03) continue;
        const turn = Math.abs(((after.angle - before.angle + 540) % 360) - 180);
        expect(
          turn,
          `${color} → ${threadInk(color, theme)} (${theme})`,
        ).toBeLessThan(8);
      }
  });

  it("reads at 4.5:1 on any background, not only the built-in themes'", () => {
    // Surfaces of every lightness, from near black to white, opaque hover and selection as
    // an Omarchy palette derives them: each colour's ink reads on all of them, keeps its hue
    // and is the colour itself where that already reads.
    let seed = 7;
    const random = () =>
      ((seed = (seed * 16807) % 2147483647) / 2147483647) * 255;
    const failures: string[] = [];
    for (let n = 0; n < 40; n++) {
      const bg = [random(), random(), random()].map(Math.round) as [
        number,
        number,
        number,
      ];
      const text =
        contrast(bg, [255, 255, 255]) > contrast(bg, [0, 0, 0])
          ? [255, 255, 255]
          : [0, 0, 0];
      const surface = (share: number) =>
        over(text as never, share, bg).map(Math.round) as [
          number,
          number,
          number,
        ];
      const hex = (c: number[]) =>
        "#" + c.map((v) => v.toString(16).padStart(2, "0")).join("");
      const surfaces: Surfaces = {
        bg: hex(bg),
        panel: hex(surface(0.06)),
        raised: hex(surface(0.11)),
        overlay: hex(surface(0.08)),
        hover: [surface(0.08), 1],
        active: [surface(0.14), 1],
      };
      // A background no colour can reach 4.5:1 on (a mid grey) is the palette's to move.
      if (Math.max(contrast(bg, [255, 255, 255]), contrast(bg, [0, 0, 0])) < 7)
        continue;
      for (const color of THREAD_COLORS) {
        const ink = threadInk(color, surfaces);
        const { ratio, on } = lowestContrast(ink, color, surfaces);
        if (ratio < MIN_CONTRAST)
          failures.push(
            `${color} on ${surfaces.bg} ${on}: ${ratio.toFixed(2)}`,
          );
        if (lowestContrast(color, color, surfaces).ratio >= 4.6)
          expect(ink).toBe(color);
        const [before, after] = [hue(color), hue(ink)];
        if (before.chroma >= 0.03 && hue(ink).chroma >= 0.03)
          expect(
            Math.abs(((after.angle - before.angle + 540) % 360) - 180),
          ).toBeLessThan(8);
      }
    }
    expect(failures).toEqual([]);
  });

  it("puts the colour and both inks on an element, and nothing for no colour", () => {
    expect(threadStyle("#ff87af")).toEqual({
      "--thread-color": "#ff87af",
      "--thread-ink-light": threadInk("#ff87af", "light"),
      "--thread-ink-dark": threadInk("#ff87af", "dark"),
      // An Omarchy palette's ink for the colour, set on the root (shared/omarchy.ts).
      "--thread-ink-omarchy": "var(--omarchy-ink-ff87af, var(--text))",
    });
    expect(threadStyle("")).toEqual({});
    expect(threadStyle(undefined)).toEqual({});
  });
});

describe("where a thread's name is drawn in its colour", () => {
  it("picks the theme's ink in CSS", () => {
    expect(rule(css, ".thread-ink")).toMatch(
      /var\(--thread-ink-light, var\(--text\)\) var\(--light-share\),\s*var\(--thread-ink-dark, var\(--text\)\)/,
    );
    // "To:" and the conversation's symbol: a thread's ink, or the orchestrator's accent.
    const shell = rule(css, ".app");
    expect(shell).toMatch(
      /--destination-ink: color-mix\(\s*in srgb,\s*var\(--destination-ink-light, var\(--destination-mix\)\) var\(--light-share\),\s*var\(--destination-ink-dark, var\(--destination-mix\)\)/,
    );
    expect(rule(css, ".composer-label b")).toContain("var(--destination-ink)");
    expect(rule(css, ".destination-symbol")).toContain(
      "var(--destination-ink)",
    );
    // --light-share: 0% in the dark theme, 100% in both light ones (explicit and system).
    expect(css.slice(0, css.indexOf("}"))).toContain("--light-share: 0%;");
    expect(rule(css, ':root[data-theme="light"]')).toContain(
      "--light-share: 100%;",
    );
    const systemAt = css.indexOf(':root[data-theme="system"] {');
    expect(css.slice(systemAt, css.indexOf("}", systemAt))).toContain(
      "--light-share: 100%;",
    );
    expect(
      rule(
        css,
        ':root[data-platform="linux"][data-theme="system"][data-omarchy="true"]',
      ),
    ).toContain("--light-share: var(--omarchy-light-share, 0%);");
    // No name or mark in the raw colour.
    expect(css).not.toMatch(/(^|[^-])color: var\(--thread-color\)/);
    expect(css).not.toMatch(/(^|[^-])color: var\(--destination\)/);
  });

  it("draws every coloured name through threadStyle and .thread-ink", () => {
    expect(app).not.toMatch(/style=\{\{\s*color:/);
    expect(app).not.toMatch(/"--thread-color":/);
    // Latest thread reports, a thread's details, a dived card's marker.
    expect(app).toMatch(
      /<span\s+className="thread-ink"\s+style=\{threadStyle\(t\.accent\)\}\s*>\s*\{t\.name\}/,
    );
    expect(app).toMatch(
      /<h2\s+className="thread-ink"\s+style=\{threadStyle\(selectedThread\.accent\)\}\s*>\s*\{selectedThread\.name\}/,
    );
    expect(app).toContain('className="dive-marker thread-ink"');
    // The "To:" chip's conversation.
    expect(app).toMatch(
      /"--destination-ink-light": threadInk\(target\.accent, "light"\)/,
    );
    expect(app).toMatch(
      /"--destination-ink-dark": threadInk\(target\.accent, "dark"\)/,
    );
  });

  it("keeps the orchestrator's own accent readable on its chip", () => {
    // The fallback ink (no thread): the accent mixed with the text, on a 12% accent tint.
    for (const theme of themes) {
      const blockAt =
        theme === "dark" ? 0 : css.indexOf(':root[data-theme="light"] {');
      const block = css.slice(blockAt, css.indexOf("}", blockAt));
      const accent = parseHex(token(block, "accent") as string)!;
      const text = parseHex(token(block, "text") as string)!;
      const strength = theme === "dark" ? 1 : 0.45;
      const ink = over(accent, strength, text);
      const chip = over(accent, 0.12, parseHex(SURFACES[theme].panel)!);
      expect(contrast(ink, chip)).toBeGreaterThanOrEqual(MIN_CONTRAST);
    }
  });
});
