import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  THREAD_COLORS,
  contrast,
  parseHex,
  threadInk,
  type RGB,
} from "../shared/threadColor";

/* Resolves the real stylesheets the way the browser does (custom properties, `var()`
 * fallbacks, `color-mix(in srgb, …)`, hex and `rgb()` colours) and lists the edges and state
 * indicators of every control the apps draw, each with the colours beside it: WCAG 2.2
 * 1.4.11 asks 3:1 of each. tests/a11y-audit.test.ts checks them in the light and the dark
 * theme, tests/omarchy-contrast.test.ts in every Omarchy palette. */

const root = join(import.meta.dirname, "..");
export const sheets = {
  style: readFileSync(join(root, "shared/style.css"), "utf8"),
  mobile: readFileSync(join(root, "mobile/src/mobile.css"), "utf8"),
  connections: readFileSync(join(root, "desktop/connections.css"), "utf8"),
  inbox: readFileSync(join(root, "shared/inbox-search.css"), "utf8"),
};

/** WCAG 2.2 1.4.11: a control's edge, a state's indicator and a focus ring. */
export const EDGE = 3;
/** WCAG 2.2 1.4.3: text. */
export const TEXT = 4.5;

/** The declarations of the first rule whose selector is exactly `selector`. */
export function block(source: string, selector: string) {
  // The rule whose whole selector is `selector`: it starts the file or follows a rule or
  // a comment.
  let at = -1;
  for (const before of ["", "}\n", "*/\n", "}\n\n"]) {
    const found = before
      ? source.indexOf(before + selector + " {")
      : source.startsWith(selector + " {")
        ? 0
        : -1;
    if (found >= 0 && (at < 0 || found + before.length < at))
      at = found + before.length;
  }
  if (at < 0) throw new Error(`no rule ${selector}`);
  return source.slice(source.indexOf("{", at) + 1, source.indexOf("}", at));
}
export function declarations(body: string) {
  const out = new Map<string, string>();
  for (const m of body
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .matchAll(/([-a-z]+)\s*:\s*([^;]+);/g))
    out.set(m[1], m[2].replace(/\s+/g, " ").trim());
  return out;
}

export type RGBA = [number, number, number, number];
/** Split `s` at its top-level commas. */
function args(s: string) {
  const out: string[] = [];
  let depth = 0,
    from = 0;
  for (let i = 0; i < s.length; i++) {
    if (s[i] === "(") depth++;
    else if (s[i] === ")") depth--;
    else if (s[i] === "," && depth === 0) {
      out.push(s.slice(from, i).trim());
      from = i + 1;
    }
  }
  out.push(s.slice(from).trim());
  return out;
}
/** A colour value as the browser computes it, with custom properties `scope`: undefined
 * when it is invalid (an unset or broken custom property). */
export function resolve(
  value: string,
  scope: Map<string, string>,
  depth = 0,
): RGBA | undefined {
  if (depth > 40) return undefined;
  const v = value.trim();
  const call = /^([a-z-]+)\(([\s\S]*)\)$/.exec(v);
  if (call?.[1] === "var") {
    const [name, ...fallback] = args(call[2]);
    const own = scope.get(name);
    const got = own === undefined ? undefined : resolve(own, scope, depth + 1);
    return (
      got ??
      (fallback.length
        ? resolve(fallback.join(","), scope, depth + 1)
        : undefined)
    );
  }
  if (call?.[1] === "color-mix") {
    const [space, a, b] = args(call[2]);
    if (space !== "in srgb" || !a || !b) return undefined;
    // A share is a percentage, or a custom property holding one (`--light-share`).
    const percent = (share: string): number | undefined => {
      const own = /^var\((--[-a-z]+)\)$/.exec(share);
      if (own) {
        const held = scope.get(own[1]);
        return held === undefined ? undefined : percent(held.trim());
      }
      const m = /^([\d.]+)%$/.exec(share);
      return m ? Number(m[1]) / 100 : undefined;
    };
    const part = (s: string) => {
      const m = /^(.*?)(?:\s+(var\(--[-a-z]+\)|[\d.]+%))?$/.exec(s)!;
      return {
        color: m[1],
        given: m[2] !== undefined,
        share: m[2] === undefined ? undefined : percent(m[2]),
      };
    };
    const [p, q] = [part(a), part(b)];
    // A share that names an unset property makes the whole mix invalid.
    if ([p, q].some((x) => x.given && x.share === undefined)) return undefined;
    const [ca, cb] = [
      resolve(p.color, scope, depth + 1),
      resolve(q.color, scope, depth + 1),
    ];
    if (!ca || !cb) return undefined;
    let [sa, sb] = [p.share, q.share];
    if (sa === undefined && sb === undefined) [sa, sb] = [0.5, 0.5];
    else if (sa === undefined) sa = 1 - sb!;
    else if (sb === undefined) sb = 1 - sa;
    const sum = sa + sb!;
    const [wa, wb] = [sa / sum, sb! / sum];
    // Premultiplied, as CSS Color 5 mixes.
    const alpha = ca[3] * wa + cb[3] * wb;
    const rgb = [0, 1, 2].map((i) =>
      alpha ? (ca[i] * ca[3] * wa + cb[i] * cb[3] * wb) / alpha : 0,
    );
    return [...rgb, alpha * Math.min(sum, 1)] as RGBA;
  }
  if (call?.[1] === "rgb") {
    // `rgb(r g b)` and `rgb(r g b / a%)`.
    const m = /^(\d+)\s+(\d+)\s+(\d+)(?:\s*\/\s*([\d.]+)(%?))?$/.exec(
      call[2].trim(),
    );
    if (!m) return undefined;
    const alpha =
      m[4] === undefined ? 1 : Number(m[4]) / (m[5] === "%" ? 100 : 1);
    return [Number(m[1]), Number(m[2]), Number(m[3]), alpha];
  }
  if (v === "transparent") return [0, 0, 0, 0];
  const rgb = parseHex(v);
  if (rgb) return [...rgb, 1];
  return undefined;
}
/** `top` composited over the opaque `bottom`. */
export const onto = (top: RGBA, bottom: RGB): RGB =>
  [0, 1, 2].map((i) => top[i] * top[3] + bottom[i] * (1 - top[3])) as RGB;
export const opaque = (c: RGBA | undefined, what: string): RGB => {
  if (!c) throw new Error(`${what} does not resolve`);
  if (c[3] < 1) throw new Error(`${what} is not opaque`);
  return [c[0], c[1], c[2]];
};

/** The custom properties on the root in the app's own dark or light theme. */
export function themeScope(theme: "dark" | "light") {
  const scope = new Map<string, string>();
  for (const [k, v] of declarations(block(sheets.style, ":root")))
    if (k.startsWith("--")) scope.set(k, v);
  if (theme === "light")
    for (const [k, v] of declarations(
      block(sheets.style, ':root[data-theme="light"]'),
    ))
      if (k.startsWith("--")) scope.set(k, v);
  return scope;
}

export interface Pair {
  what: string;
  ratio: number;
  min: number;
}
export const failing = (list: Pair[]) =>
  list
    .filter((p) => p.ratio < p.min)
    .map((p) => `${p.what}: ${p.ratio.toFixed(2)}:1`);

/** The colour in a `border`, `outline` or `box-shadow` value (its last part). */
function colorOf(value: string) {
  const m = /(var\(.*\)|color-mix\(.*\)|#[0-9a-f]{6}|transparent)$/i.exec(
    value,
  );
  if (!m) throw new Error(`no colour in ${value}`);
  return m[1];
}

/** The properties App.tsx sets on `.app` for a destination (a thread's colour, or none for
 * the orchestrator), in the app's own themes. */
function builtInDestination(color: string | undefined): [string, string][] {
  return color
    ? [
        ["--destination", color],
        ["--destination-ink-light", threadInk(color, "light")],
        ["--destination-ink-dark", threadInk(color, "dark")],
      ]
    : [["--destination", "var(--accent)"]];
}

/** Every control edge and state indicator the apps draw with the root properties `scope`,
 * each against the colours beside it. `destination` gives the properties `.app` holds for
 * a destination, after the stylesheet's own `.app` rule. */
export function controlPairs(
  scope: Map<string, string>,
  destination: (
    color: string | undefined,
  ) => [string, string][] = builtInDestination,
): Pair[] {
  const out: Pair[] = [];
  const decl = (source: string, selector: string, property: string) => {
    const value = declarations(block(source, selector)).get(property);
    if (!value) throw new Error(`${selector} has no ${property}`);
    return value;
  };
  const style = (selector: string, property: string) =>
    decl(sheets.style, selector, property);
  /** A colour value (or a declaration's colour), composited over `base`. */
  const paint = (value: string, base: RGB, local = scope): RGB => {
    const color = resolve(value, local);
    if (!color) throw new Error(`${value} does not resolve`);
    return onto(color, base);
  };
  const token = (name: string) =>
    opaque(resolve(`var(--${name})`, scope), name);
  const check = (what: string, fg: RGB, bg: RGB, min = EDGE) =>
    out.push({ what, ratio: contrast(fg, bg), min });
  /** One control's checks: a rule it needs that is missing (or reads otherwise) fails it. */
  const part = (name: string, run: () => void) => {
    try {
      run();
    } catch (error) {
      out.push({
        what: `${name}: ${(error as Error).message}`,
        ratio: 0,
        min: EDGE,
      });
    }
  };

  // The surfaces a control sits on, and the field's own fill.
  const bases: [string, RGB][] = (
    ["bg", "panel", "raised", "overlay", "bubble"] as const
  ).map((name) => [name, token(name)]);
  const on = (name: string) => bases.find(([n]) => n === name)![1];

  part("fields", () => {
    // A field (input, textarea, select, the search boxes), on every surface and on its fill.
    expect(
      style("input,\ntextarea,\nselect", "border"),
      "1px solid var(--field-border)",
    );
    expect(style("input,\ntextarea,\nselect", "background"), "var(--bg)");
    for (const [where, bg] of bases)
      check(`field edge on ${where}`, token("field-border"), bg);
  });
  part("answer box and composer", () => {
    // A request's answer box (on its card, the panel) and the composer (its fill is the
    // panel; it sits on the page or the panel).
    const answer = paint(
      colorOf(style(".attention-response > input", "border")),
      on("bg"),
    );
    for (const where of ["bg", "panel"])
      check(`answer box edge on ${where}`, answer, on(where));
    const composer = paint(style(".composer", "border-color"), on("panel"));
    for (const where of ["bg", "panel"])
      check(`composer edge on ${where}`, composer, on(where));
  });
  part("focused composer", () => {
    // Focused, the composer's edge takes some of its destination's colour: the
    // orchestrator's accent or a thread's colour.
    const app = [...declarations(block(sheets.style, ".app"))].filter(([k]) =>
      k.startsWith("--"),
    );
    for (const color of [undefined, ...THREAD_COLORS]) {
      const name = color ?? "the orchestrator";
      const local = new Map([...scope, ...app, ...destination(color)]);
      const edge = paint(
        style(".composer:focus-within", "border-color"),
        on("panel"),
        local,
      );
      for (const where of ["bg", "panel"])
        check(`focused composer edge (${name}) on ${where}`, edge, on(where));
    }
  });
  part("palette field", () => {
    // The command palette's field: the line under it, in the palette (overlay).
    check(
      "palette field edge on overlay",
      paint(colorOf(style(".palette-search", "border-bottom")), on("overlay")),
      on("overlay"),
    );
  });
  part("checkbox", () => {
    // A checkbox (and a switch, drawn as one): its edge on every surface and on its fill;
    // checked, its fill on every surface and its mark on the fill.
    const box = 'input[type="checkbox"]';
    expect(style(box, "appearance"), "none");
    const boxFill = paint(style(box, "background"), on("bg"));
    const boxEdge = paint(colorOf(style(box, "border")), boxFill);
    check("checkbox edge on its fill", boxEdge, boxFill);
    for (const [where, bg] of bases)
      check(`checkbox edge on ${where}`, boxEdge, bg);
    const checked = paint(style(`${box}:checked`, "background"), on("bg"));
    expect(
      style(`${box}:checked`, "border-color"),
      style(`${box}:checked`, "background"),
    );
    for (const [where, bg] of bases)
      check(`checked checkbox on ${where}`, checked, bg);
    check(
      "checkbox's mark on its checked fill",
      paint(colorOf(style(`${box}:checked::before`, "border")), checked),
      checked,
    );
  });
  part("slider", () => {
    // A slider: its track on every surface; its thumb on its ring, which is the page.
    const range = 'input[type="range"]';
    expect(style(range, "appearance"), "none");
    const track = paint(
      style(`${range}::-webkit-slider-runnable-track`, "background"),
      on("bg"),
    );
    for (const [where, bg] of bases)
      check(`slider track on ${where}`, track, bg);
    const thumb = `${range}::-webkit-slider-thumb`;
    const ring = paint(colorOf(style(thumb, "border")), on("bg"));
    const knob = paint(style(thumb, "background"), ring);
    check("slider thumb on its ring", knob, ring);
    for (const where of ["bg", "panel"])
      check(`slider thumb on ${where}`, knob, on(where));
  });
  part("segmented control", () => {
    // A segmented control (Appearance, a loop's preset and Save for): the selected segment's
    // edge on its fill and on the group's.
    const group = paint(style(".segmented", "background"), on("bg"));
    const segment = paint(
      style(".segmented > button.active", "background"),
      group,
    );
    const segmentEdge = paint(
      style(".segmented > button.active", "border-color"),
      segment,
    );
    check("selected segment's edge on its fill", segmentEdge, segment);
    check("selected segment's edge on the group", segmentEdge, group);
  });
  part("tabs", () => {
    // A tab (Hubs and remote access): the selected tab's edge on the dialog and on its fill,
    // and its words on its fill.
    const tab = '.connections-content [role="tab"][aria-selected="true"]';
    const tabFill = paint(
      decl(sheets.connections, tab, "background"),
      on("overlay"),
    );
    const tabEdge = paint(
      decl(sheets.connections, tab, "border-color"),
      tabFill,
    );
    check("selected tab's edge on the dialog", tabEdge, on("overlay"));
    check("selected tab's edge on its fill", tabEdge, tabFill);
    check(
      "selected tab's words on its fill",
      paint(decl(sheets.connections, tab, "color"), tabFill),
      tabFill,
      TEXT,
    );
  });
  part("selected thread card", () => {
    // The selected thread card: its edge on the list around it (the sidebar's panel, the
    // grid's page). Its own fill (the selected tint) is not what the edge parts it from.
    check(
      "selected thread card edge on panel",
      paint(style(".thread-card.selected", "border-color"), on("panel")),
      on("panel"),
    );
    check(
      "selected grid card edge on bg",
      paint(style(".thread-card.large.selected", "border-color"), on("bg")),
      on("bg"),
    );
    check(
      "selected dived card edge on panel",
      paint(style(".thread-card.dived.selected", "border-color"), on("panel")),
      on("panel"),
    );
  });
  part("selected list item", () => {
    // The selected item of a list or a navigation (the primary navigation, a workspace's
    // Orchestrator, Manage, the palette's and the mention list's current option): a bar on
    // its fill.
    for (const [selector, where] of [
      [
        ".primary-nav > button.active,\n.primary-nav > button.active:hover",
        "panel",
      ],
      [
        ".workspace-orchestrator.active,\n.workspace-orchestrator.active:hover",
        "panel",
      ],
      [
        ".mentions > button.active,\n.palette-list > button.active,\n.palette-options > button.active",
        "overlay",
      ],
      [
        ".mentions > button.active,\n.palette-list > button.active,\n.palette-options > button.active",
        "raised",
      ],
    ] as const) {
      const fill = paint(style(selector, "background"), on(where));
      check(
        `${selector.split(",")[0]}'s bar on its fill (${where})`,
        paint(colorOf(style(selector, "box-shadow")), fill),
        fill,
      );
    }
    {
      const fill = paint(
        style(
          ".manage-nav button.active,\n.manage-nav button:hover",
          "background",
        ),
        on("panel"),
      );
      check(
        ".manage-nav button.active's bar on its fill",
        paint(colorOf(style(".manage-nav button.active", "box-shadow")), fill),
        fill,
      );
    }
  });
  part("pressed icon toggle", () => {
    // A pressed icon toggle (Toggle tool calls, Toggle reasoning, Focus): its edge on its
    // fill, where it sits on the page or the panel.
    for (const where of ["bg", "panel"]) {
      const fill = paint(
        style(".icon-button:hover,\n.icon-button.on", "background"),
        on(where),
      );
      check(
        `pressed icon toggle's edge on its fill (${where})`,
        paint(colorOf(style(".icon-button.on", "box-shadow")), fill),
        fill,
      );
    }
  });
  part("menu toggle", () => {
    // A narrow window's menu: an on toggle's check, on the menu.
    check(
      "conversation menu's check on the menu",
      paint(
        colorOf(
          style(
            '.conversation-menu > div > button[aria-pressed="true"]::after',
            "border",
          ),
        ),
        on("overlay"),
      ),
      on("overlay"),
    );
  });
  part("pressed filter and microphone", () => {
    // A pressed filter (Needs you › Request type) and the recording microphone: their fills.
    const filter = paint(
      decl(
        sheets.inbox,
        '.inbox-filters button[aria-pressed="true"]',
        "background",
      ),
      on("bg"),
    );
    check("pressed request filter on bg", filter, on("bg"));
    check(
      "recording microphone on the composer",
      paint(style(".voice-button.recording", "background"), on("panel")),
      on("panel"),
    );
  });
  part("phone's tab bar", () => {
    // The phone's tab bar: the current tab's pill, its edge on its fill and on the bar.
    const pillRule = '.mobile-tabs button[aria-current="page"]::before';
    const pill = paint(
      decl(sheets.mobile, pillRule, "background"),
      on("panel"),
    );
    const pillEdge = paint(
      colorOf(decl(sheets.mobile, pillRule, "box-shadow")),
      pill,
    );
    check("phone's current tab edge on its pill", pillEdge, pill);
    check("phone's current tab edge on the bar", pillEdge, on("panel"));
  });
  part("terminal", () => {
    // The terminal keeps its dark palette in every theme: its search field's edge, the
    // current tab's underline, the phone's pressed Ctrl, and its focus ring.
    const terminalBg = token("terminal-bg");
    const terminalPanel = token("terminal-panel");
    const terminalField = paint(
      style(".terminal-search > input", "border-color"),
      terminalPanel,
    );
    check(
      "terminal search field edge on its fill",
      terminalField,
      terminalPanel,
    );
    check(
      "terminal search field edge on the terminal",
      terminalField,
      terminalBg,
    );
    const terminalTab = paint(
      style(".terminal-tab.active", "background"),
      terminalPanel,
    );
    check(
      "terminal's current tab underline on the tab",
      paint(colorOf(style(".terminal-tab.active", "box-shadow")), terminalTab),
      terminalTab,
    );
    const ctrl = '.terminal-accessory [aria-pressed="true"]';
    const ctrlFill = paint(
      decl(sheets.mobile, ctrl, "background"),
      terminalPanel,
    );
    const ctrlEdge = paint(decl(sheets.mobile, ctrl, "border-color"), ctrlFill);
    check("pressed Ctrl's edge on its fill", ctrlEdge, ctrlFill);
    check("pressed Ctrl's edge on the terminal", ctrlEdge, terminalPanel);
    const terminalScope = new Map([
      ...scope,
      ["--focus", style(".terminal-dock,\n.terminal-session", "--focus")],
    ]);
    for (const [where, bg] of [
      ["terminal", terminalBg],
      ["terminal panel", terminalPanel],
    ] as const)
      check(
        `focus ring in the terminal on its ${where}`,
        paint("var(--focus)", bg, terminalScope),
        bg,
      );
  });
  part("focus ring", () => {
    // The focus ring, on every surface and tint: buttons, links and summaries use it too.
    expect(
      style(
        "button:focus-visible,\na:focus-visible,\nsummary:focus-visible",
        "outline",
      ),
      "2px solid var(--focus)",
    );
    const tints = [
      "hover",
      "active",
      "accent-soft",
      "warning-soft",
      "danger-soft",
      "loop-soft",
    ];
    const focus = token("focus");
    for (const [where, base] of bases) {
      check(`focus ring on ${where}`, focus, base);
      for (const tint of tints)
        check(
          `focus ring on ${tint} over ${where}`,
          focus,
          paint(`var(--${tint})`, base),
        );
    }
  });
  return out;
}

function expect(got: string, want: string) {
  if (got !== want)
    throw new Error(`expected ${want}, the stylesheet has ${got}`);
}
