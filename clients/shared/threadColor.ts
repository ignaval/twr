/* A thread's colour, and the variant of it its name is written in.
 *
 * The hub gives each thread one of 30 colours (`twr_core::palette`, sent as the thread's
 * `accent`). The colour itself marks the thread wherever it is a dot, a band, a border or a
 * tint. Some are too light to read as text on the light theme (and a few too dark on the
 * dark one), so where the thread's NAME is drawn in its colour it uses the colour's ink: the
 * same hue, darkened (light theme) or lightened (dark theme) only as far as it takes to reach
 * 4.5:1 against every background a name sits on, tints of the colour itself included.
 *
 * `threadStyle` puts the colour and both inks on an element as custom properties
 * (`--thread-color`, `--thread-ink-light`, `--thread-ink-dark`); CSS picks the ink for the
 * theme in use with `--light-share` (style.css, `.thread-ink`). */
import type { CSSProperties } from "react";

/** `twr_core::palette::THREAD_PALETTE`: xterm 256-colour cube indices. */
export const THREAD_PALETTE = [
  31, 201, 106, 211, 130, 99, 111, 28, 27, 73, 97, 169, 129, 100, 104, 95, 199,
  171, 206, 134, 34, 29, 62, 141, 204, 39, 36, 69, 146, 131,
] as const;

/** The RGB xterm draws cube index `n` with, as `#rrggbb` (`twr_core::palette::cube_rgb`). */
export function cubeHex(n: number) {
  const levels = [0, 95, 135, 175, 215, 255];
  const i = Math.min(Math.max(n, 16), 231) - 16;
  return toHex([
    levels[Math.floor(i / 36)],
    levels[Math.floor(i / 6) % 6],
    levels[i % 6],
  ]);
}

/** The 30 thread colours as the hub sends them. */
export const THREAD_COLORS = THREAD_PALETTE.map(cubeHex);

/** WCAG's minimum for normal text, and the ink's target (a little over it, so a background
 * composited to 8 bits on screen cannot round it under). */
export const MIN_CONTRAST = 4.5;
export const TARGET = 4.6;

export type Theme = "light" | "dark";

export type RGB = [number, number, number];

/** The backgrounds a name can sit on: four surfaces, and the hover and selected overlays
 * (a colour at an opacity; 1 for the opaque ones an Omarchy palette derives). */
export interface Surfaces {
  bg: string;
  panel: string;
  raised: string;
  overlay: string;
  hover: [RGB, number];
  active: [RGB, number];
}

/** The theme tokens a name can sit on (style.css: `:root` and `:root[data-theme="light"]`;
 * a test keeps them equal). */
export const SURFACES: Record<Theme, Surfaces> = {
  dark: {
    bg: "#0e1013",
    panel: "#13161a",
    raised: "#1a1e23",
    overlay: "#1f242a",
    hover: [[255, 255, 255], 0.045],
    active: [[255, 255, 255], 0.075],
  },
  light: {
    bg: "#fbfbfa",
    panel: "#f4f4f2",
    raised: "#ffffff",
    overlay: "#ffffff",
    hover: [[0, 0, 0], 0.04],
    active: [[0, 0, 0], 0.07],
  },
};

/** How much of the thread's colour a tinted background holds: a dived card (8%,
 * `.thread-card.dived`), the composer's "To:" chip and the conversation's symbol (12%,
 * `.destination-chip`, `.destination-symbol.worker`). */
export const TINTS = [0.08, 0.12];

export function parseHex(value: string): RGB | undefined {
  const m = /^#([0-9a-f]{6})$/i.exec(value.trim());
  if (!m) return undefined;
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
export function toHex(rgb: readonly number[]) {
  return (
    "#" +
    rgb
      .map((v) =>
        Math.round(Math.min(Math.max(v, 0), 255))
          .toString(16)
          .padStart(2, "0"),
      )
      .join("")
  );
}
/** `top` at opacity `alpha` over the opaque `bottom`. */
export function over(top: RGB, alpha: number, bottom: RGB): RGB {
  return [0, 1, 2].map((i) => top[i] * alpha + bottom[i] * (1 - alpha)) as RGB;
}
const linear = (v: number) => {
  const s = v / 255;
  return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
};
export function luminance(c: RGB) {
  return 0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2]);
}
export function contrast(a: RGB, b: RGB) {
  const [high, low] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (high + 0.05) / (low + 0.05);
}

/** Every background a thread's name is drawn on in `theme` (a built-in theme, or the
 * surfaces of a palette taken from the desktop), for the thread coloured `color`, named by
 * where it is (for a failing test to say). */
export function backgrounds(
  theme: Theme | Surfaces,
  color: RGB,
): [string, RGB][] {
  const s = typeof theme === "string" ? SURFACES[theme] : theme;
  const out: [string, RGB][] = [];
  for (const base of ["bg", "panel", "raised", "overlay"] as const) {
    const plain = parseHex(s[base])!;
    out.push([base, plain]);
    out.push([`${base}+hover`, over(s.hover[0], s.hover[1], plain)]);
    // A selected card (`.thread-card.selected`).
    out.push([`${base}+selected`, over(s.active[0], s.active[1], plain)]);
    for (const tint of TINTS) {
      const tinted = over(color, tint, plain);
      out.push([`${base}+${tint * 100}% tint`, tinted]);
      out.push([
        `${base}+${tint * 100}% tint+hover`,
        over(s.hover[0], s.hover[1], tinted),
      ]);
    }
  }
  return out;
}

// OKLab (Björn Ottosson), to change a colour's lightness and keep its hue.
function toOklab(c: RGB): RGB {
  const [r, g, b] = c.map(linear);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [
    0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
  ];
}
/** OKLab to sRGB (0–255, unclamped: out of gamut when a channel leaves 0–255). */
function fromOklab([L, a, b]: RGB): RGB {
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const lin = [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
  return lin.map((v) => {
    const s2 = v <= 0.0031308 ? 12.92 * v : 1.055 * v ** (1 / 2.4) - 0.055;
    return s2 * 255;
  }) as RGB;
}
const inGamut = (c: RGB) => c.every((v) => v >= -0.5 && v <= 255.5);
/** The colour at OKLab lightness `L` with `color`'s hue, as much of its chroma as sRGB
 * holds there. */
function atLightness(L: number, a: number, b: number): RGB {
  let [low, high] = [0, 1];
  if (inGamut(fromOklab([L, a, b])))
    return fromOklab([L, a, b]).map(Math.round) as RGB;
  for (let i = 0; i < 24; i++) {
    const k = (low + high) / 2;
    if (inGamut(fromOklab([L, a * k, b * k]))) low = k;
    else high = k;
  }
  return fromOklab([L, a * low, b * low]).map((v) =>
    Math.round(Math.min(Math.max(v, 0), 255)),
  ) as RGB;
}

/** The lowest contrast a colour of luminance `ink` has on any of `against`. */
function worstOf(ink: number, against: readonly number[]) {
  let low = Infinity;
  for (const bg of against)
    low = Math.min(
      low,
      (Math.max(ink, bg) + 0.05) / (Math.min(ink, bg) + 0.05),
    );
  return low;
}

const WHITE: RGB = [255, 255, 255];
const BLACK: RGB = [0, 0, 0];

/** `color` as it must be drawn to read at `target`:1 on every one of `against`: `color`
 * itself when it already does, else the nearest colour of the same hue (OKLab hue, as much
 * of its chroma as sRGB holds) whose lightness reaches it. `direction` fixes the way the
 * lightness may move (1 lighter, -1 darker); without it the smaller move wins. When no
 * lightness reaches the target, the black or white that comes closest. A colour that
 * already reads at `keep`:1 (the target unless given) is kept as it is. */
export function readableOn(
  color: RGB,
  against: readonly RGB[],
  target: number,
  direction?: 1 | -1,
  keep = target,
): RGB {
  const lums = against.map(luminance);
  const worst = (c: RGB) => worstOf(luminance(c), lums);
  if (worst(color) >= keep) return color;
  const [L, a, b] = toOklab(color);
  let best: { ink: RGB; move: number } | undefined;
  for (const sign of direction ? [direction] : ([1, -1] as const)) {
    let found = false;
    for (let l = L + sign * 0.002; l > 0 && l < 1; l += sign * 0.002) {
      const ink = atLightness(l, a, b);
      if (worst(ink) >= target) {
        const move = Math.abs(l - L);
        if (!best || move < best.move) best = { ink, move };
        found = true;
        break;
      }
    }
    const end = sign > 0 ? WHITE : BLACK;
    if (!found && !best && worst(end) >= target) best = { ink: end, move: 1 };
  }
  if (best) return best.ink;
  return worst(WHITE) >= worst(BLACK) ? WHITE : BLACK;
}

const cache = new Map<string, string>();
/** The colour `color` (`#rrggbb`) is written in as text in `theme` (a built-in theme, or a
 * palette's surfaces): `color` itself when it already reads at 4.5:1 on every background a
 * name sits on there, else the nearest colour of the same hue that does (darker in the
 * light theme, lighter in the dark one; on other surfaces, the smaller move). */
export function threadInk(color: string, theme: Theme | Surfaces): string {
  const key =
    (typeof theme === "string" ? theme : JSON.stringify(theme)) +
    color.toLowerCase();
  const hit = cache.get(key);
  if (hit) return hit;
  const rgb = parseHex(color);
  if (!rgb) return color;
  const against = backgrounds(theme, rgb).map(([, bg]) => bg);
  const direction = theme === "light" ? -1 : theme === "dark" ? 1 : undefined;
  const out = toHex(readableOn(rgb, against, TARGET, direction));
  if (cache.size > 2000) cache.clear();
  cache.set(key, out);
  return out;
}

/** The root custom property an Omarchy palette sets for the ink of thread colour `color`
 * (shared/omarchy.ts), as a `var()` that falls back to the palette's text. */
export function omarchyInkVar(color: string) {
  return `var(--omarchy-ink-${color.slice(1).toLowerCase()}, var(--text))`;
}

/** The lowest contrast `ink` has on the backgrounds a name sits on in `theme` for a thread
 * coloured `color`, and which background that is. */
export function lowestContrast(
  ink: string,
  color: string,
  theme: Theme | Surfaces,
) {
  const against = backgrounds(theme, parseHex(color)!);
  let low: { ratio: number; on: string } = { ratio: Infinity, on: "" };
  for (const [on, bg] of against) {
    const ratio = contrast(parseHex(ink)!, bg);
    if (ratio < low.ratio) low = { ratio, on };
  }
  return low;
}

/** The custom properties an element drawn in a thread's colour carries: the colour, the
 * ink of each built-in theme for its name, and the Omarchy palette's ink (set on the root
 * by the palette, `.thread-ink`). No colour (an old cached row): none. */
export function threadStyle(
  color: string | undefined,
  prefix = "thread",
): CSSProperties {
  if (!color || !parseHex(color)) return {};
  return {
    [`--${prefix}-color`]: color,
    [`--${prefix}-ink-light`]: threadInk(color, "light"),
    [`--${prefix}-ink-dark`]: threadInk(color, "dark"),
    [`--${prefix}-ink-omarchy`]: omarchyInkVar(color),
  } as CSSProperties;
}
