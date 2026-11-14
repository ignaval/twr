/* An Omarchy theme, as the desktop app draws it on Linux.
 *
 * The app reads the selected theme's `colors.toml` (`parseOmarchyColors`) and derives every
 * colour token its stylesheet uses from it (`omarchyPalette`): the surfaces are mixed from
 * the theme's background and foreground as before, and each colour that text, an icon, a
 * focus ring or a field's edge is drawn in keeps its hue and moves its lightness only as far
 * as it takes to read on every background it is drawn on: 4.5:1 for text, 3:1 for a field's
 * edge. A colour that already reads is used exactly as the theme gives it. App.tsx puts the
 * result on the root as `--omarchy-*` properties, and style.css's Omarchy block maps them
 * onto the tokens. */
import {
  THREAD_COLORS,
  TARGET,
  contrast,
  luminance,
  over,
  parseHex,
  readableOn,
  threadInk,
  toHex,
  type RGB,
  type Surfaces,
} from "./threadColor";

const KEY =
  /^\s*(background|foreground|accent|selection|selection_background|muted|dark_background|darker_background|lighter_background|red|green|yellow|blue|magenta|cyan|bright_(?:red|green|yellow|blue|magenta|cyan)|color[0-9]+)\s*=\s*["'](#[a-fA-F0-9]{6})["']/;

/** The colours of an Omarchy `colors.toml` the app knows, and its `mode`; null without a
 * background and a foreground. */
export function parseOmarchyColors(
  text: string,
): Record<string, string> | null {
  const colors: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const match = line.match(KEY);
    if (match) colors[match[1]] = match[2];
  }
  const mode = text.match(/^\s*mode\s*=\s*["'](light|dark)["']/m);
  if (mode) colors.mode = mode[1];
  return colors.background && colors.foreground ? colors : null;
}

/** WCAG 2.2 AA: text (1.4.3), and a control's edge or focus ring (1.4.11). */
export const TEXT_CONTRAST = 4.5;
export const EDGE_CONTRAST = 3;

/** The tokens a palette sets (style.css's names, without `--`). */
export const PALETTE_TOKENS = [
  "bg",
  "panel",
  "raised",
  "overlay",
  "bubble",
  "hover",
  "active",
  "border",
  "border-strong",
  "text",
  "muted",
  "faint",
  "accent",
  "accent-fg",
  "accent-soft",
  "warning",
  "warning-soft",
  "danger",
  "danger-soft",
  "danger-fg",
  "success",
  "loop",
  "loop-soft",
  "selection",
] as const;
export type PaletteToken = (typeof PALETTE_TOKENS)[number];

export interface OmarchyPalette {
  scheme: "light" | "dark";
  tokens: Record<PaletteToken, string>;
  /** The ink each of the hub's thread colours is written in (`#rrggbb` → `#rrggbb`). */
  inks: Record<string, string>;
}

/** The colours text is drawn in, on any surface. */
export const TEXT_TOKENS = [
  "text",
  "muted",
  "faint",
  "accent",
  "warning",
  "danger",
  "success",
  "loop",
] as const;

const WHITE: RGB = [255, 255, 255];
const BLACK: RGB = [0, 0, 0];
/** CSS `color-mix(in srgb, a share, b)`. */
const mix = (a: RGB, share: number, b: RGB): RGB =>
  over(a, share, b).map(Math.round) as RGB;
const valid = (value: unknown) =>
  typeof value === "string" ? parseHex(value) : undefined;
const same = (a: RGB, b: RGB) => a.every((v, i) => v === b[i]);

export type TextToken = (typeof TEXT_TOKENS)[number];

/** Every background text in `token` is drawn on, for a palette with these colours: the
 * surfaces and soft tints, and a dived card (a thread colour at 8% over the panel) for all;
 * the "To:" chip in the composer (the accent or a thread colour at 12% over the panel) for
 * the words beside the name; a success pill (success at 12% over a surface) for success. */
export function textBackgrounds(
  token: TextToken,
  t: {
    bg: RGB;
    panel: RGB;
    raised: RGB;
    overlay: RGB;
    bubble: RGB;
    hover: RGB;
    active: RGB;
    accentSoft: RGB;
    warningSoft: RGB;
    dangerSoft: RGB;
    loopSoft: RGB;
    accent: RGB;
    success: RGB;
  },
): RGB[] {
  const out = [
    t.bg,
    t.panel,
    t.raised,
    t.overlay,
    t.bubble,
    t.hover,
    t.active,
    t.accentSoft,
    t.warningSoft,
    t.dangerSoft,
    t.loopSoft,
  ];
  const threads = THREAD_COLORS.map((c) => parseHex(c)!);
  for (const color of threads) out.push(over(color, 0.08, t.panel));
  if (token === "text" || token === "muted" || token === "faint") {
    out.push(over(t.accent, 0.12, t.panel));
    for (const color of threads) out.push(over(color, 0.12, t.panel));
  }
  if (token === "accent") out.push(over(t.accent, 0.12, t.panel));
  if (token === "success")
    for (const base of [t.bg, t.panel, t.raised, t.overlay])
      out.push(over(t.success, 0.12, base));
  return out;
}

/** The palette the app draws with for an Omarchy theme (`parseOmarchyColors`' record), or
 * null when it has no usable background and foreground. */
export function omarchyPalette(
  theme: Record<string, unknown> | null | undefined,
): OmarchyPalette | null {
  if (!theme || typeof theme !== "object") return null;
  let bg = valid(theme.background);
  const fg = valid(theme.foreground);
  if (!bg || !fg) return null;
  // A background no text reads well on (a mid grey) moves towards its nearer end until
  // black or white reads at 7:1 on it, so the surfaces mixed towards the text keep room for
  // 4.5:1. No stock theme comes near it.
  const darkBackground = contrast(bg, WHITE) >= contrast(bg, BLACK);
  if (Math.max(contrast(bg, WHITE), contrast(bg, BLACK)) < 7)
    bg = readableOn(bg, [darkBackground ? WHITE : BLACK], 7.05, undefined, 7);
  const scheme =
    theme.mode === "light" || theme.mode === "dark"
      ? theme.mode
      : (bg[0] * 299 + bg[1] * 587 + bg[2] * 114) / 1000 > 140
        ? "light"
        : "dark";
  const accentIn =
    valid(theme.accent) ??
    (darkBackground ? ([98, 199, 212] as RGB) : ([7, 108, 124] as RGB));
  const warningIn = valid(theme.yellow) ?? valid(theme.color3);
  const dangerIn = valid(theme.red) ?? valid(theme.color1);
  const selectionIn =
    valid(theme.selection) ?? valid(theme.selection_background);

  // The surfaces mix towards the text and the soft tints towards their colours, and those
  // colours are adjusted against the surfaces: start from the theme's own colours and
  // settle (two or three rounds; a colour that reads on the first is never moved).
  let state = {
    text: fg,
    accent: accentIn,
    warning: warningIn ?? mix(fg, 0.8, [214, 162, 47]),
    danger: dangerIn ?? mix(fg, 0.7, [219, 75, 75]),
    success: mix(fg, 0.7, [59, 156, 110]),
    loop: mix(fg, 0.7, [143, 111, 224]),
  };
  let surfaces = derive(bg, state);
  for (let round = 0; round < 12; round++) {
    const ink = (token: TextToken, color: RGB) =>
      readableOn(
        color,
        textBackgrounds(token, surfaces),
        TARGET,
        undefined,
        TEXT_CONTRAST,
      );
    const text = ink("text", fg);
    const next = {
      text,
      accent: ink("accent", accentIn),
      warning: ink("warning", warningIn ?? mix(text, 0.8, [214, 162, 47])),
      danger: ink("danger", dangerIn ?? mix(text, 0.7, [219, 75, 75])),
      success: ink("success", mix(text, 0.7, [59, 156, 110])),
      loop: ink("loop", mix(text, 0.7, [143, 111, 224])),
    };
    const settled = (Object.keys(next) as (keyof typeof next)[]).every((key) =>
      same(next[key], state[key]),
    );
    state = next;
    surfaces = derive(bg, state);
    if (settled) break;
  }
  const ink = (token: TextToken, color: RGB) =>
    readableOn(
      color,
      textBackgrounds(token, surfaces),
      TARGET,
      undefined,
      TEXT_CONTRAST,
    );
  const muted = ink("muted", mix(state.text, 0.85, bg));
  const faint = ink("faint", mix(state.text, 0.8, bg));
  // A filled button's label, on its fill and on the fill's hover (`.primary`, `.danger`,
  // the microphone while it records).
  const accentHover = mix(state.accent, 0.9, state.text);
  const dangerHover = mix(state.danger, 0.88, state.text);
  const accentFg = readableOn(
    luminance(state.accent) > 0.179 ? [6, 25, 28] : WHITE,
    [state.accent, accentHover, state.danger],
    TARGET,
    undefined,
    TEXT_CONTRAST,
  );
  const dangerFg = readableOn(
    bg,
    [state.danger, dangerHover],
    TARGET,
    undefined,
    TEXT_CONTRAST,
  );
  // Selected text is drawn in the text colour on the theme's selection colour.
  const selection = readableOn(
    selectionIn ?? mix(bg, 0.7, state.accent),
    [state.text],
    TARGET,
    undefined,
    TEXT_CONTRAST,
  );
  // A field's edge (and the composer's), on the field and around it.
  const borderStrong = readableOn(
    mix(bg, 0.65, state.text),
    [
      surfaces.bg,
      surfaces.panel,
      surfaces.raised,
      surfaces.overlay,
      surfaces.bubble,
      surfaces.hover,
      surfaces.active,
    ],
    EDGE_CONTRAST + 0.1,
    undefined,
    EDGE_CONTRAST,
  );
  const inkSurfaces: Surfaces = {
    bg: toHex(surfaces.bg),
    panel: toHex(surfaces.panel),
    raised: toHex(surfaces.raised),
    overlay: toHex(surfaces.overlay),
    hover: [surfaces.hover, 1],
    active: [surfaces.active, 1],
  };
  const inks: Record<string, string> = {};
  for (const color of THREAD_COLORS)
    inks[color] = threadInk(color, inkSurfaces);
  const tokens: Record<PaletteToken, RGB> = {
    bg: surfaces.bg,
    panel: surfaces.panel,
    raised: surfaces.raised,
    overlay: surfaces.overlay,
    bubble: surfaces.bubble,
    hover: surfaces.hover,
    active: surfaces.active,
    border: mix(bg, 0.8, state.text),
    "border-strong": borderStrong,
    text: state.text,
    muted,
    faint,
    accent: state.accent,
    "accent-fg": accentFg,
    "accent-soft": surfaces.accentSoft,
    warning: state.warning,
    "warning-soft": surfaces.warningSoft,
    danger: state.danger,
    "danger-soft": surfaces.dangerSoft,
    "danger-fg": dangerFg,
    success: state.success,
    loop: state.loop,
    "loop-soft": surfaces.loopSoft,
    selection,
  };
  return {
    scheme,
    tokens: Object.fromEntries(
      Object.entries(tokens).map(([key, value]) => [key, toHex(value)]),
    ) as Record<PaletteToken, string>,
    inks,
  };
}

/** The surfaces and soft tints (style.css's earlier Omarchy mixes) for these colours. */
function derive(
  bg: RGB,
  c: {
    text: RGB;
    accent: RGB;
    warning: RGB;
    danger: RGB;
    success: RGB;
    loop: RGB;
  },
) {
  return {
    bg,
    panel: mix(bg, 0.94, c.text),
    raised: mix(bg, 0.89, c.text),
    overlay: mix(bg, 0.92, c.text),
    bubble: mix(bg, 0.9, c.text),
    hover: mix(bg, 0.92, c.text),
    active: mix(bg, 0.86, c.text),
    accentSoft: mix(bg, 0.87, c.accent),
    warningSoft: mix(bg, 0.9, c.warning),
    dangerSoft: mix(bg, 0.9, c.danger),
    loopSoft: mix(bg, 0.88, c.loop),
    accent: c.accent,
    success: c.success,
  };
}

/** The custom properties App.tsx sets on the root for an Omarchy theme, or null when the
 * theme has no usable background and foreground (the app's own themes apply). */
export function omarchyProperties(
  theme: Record<string, unknown> | null | undefined,
): Record<string, string> | null {
  const palette = omarchyPalette(theme);
  if (!palette) return null;
  const properties: Record<string, string> = {
    "--omarchy-scheme": palette.scheme,
    "--omarchy-light-share": palette.scheme === "light" ? "100%" : "0%",
  };
  for (const [token, value] of Object.entries(palette.tokens))
    properties[`--omarchy-${token}`] = value;
  for (const [color, ink] of Object.entries(palette.inks))
    properties[`--omarchy-ink-${color.slice(1)}`] = ink;
  return properties;
}
