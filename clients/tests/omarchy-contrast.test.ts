import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  EDGE_CONTRAST,
  TEXT_CONTRAST,
  omarchyPalette,
  omarchyProperties,
  parseOmarchyColors,
} from "../shared/omarchy";
import {
  THREAD_COLORS,
  backgrounds,
  contrast,
  omarchyInkVar,
  parseHex,
  threadStyle,
  type RGB,
} from "../shared/threadColor";
import { STOCK_THEMES } from "./omarchy-themes";

/* On Linux the desktop app draws with the selected Omarchy theme. This resolves the real
 * stylesheet (style.css's `:root` and Omarchy blocks, with the properties App.tsx puts on
 * the root for the theme) the way the browser does, and checks every text and background
 * pair the app draws, for every stock Omarchy theme and for broken ones: 4.5:1 for text,
 * 3:1 for a field's edge and a focus ring. */

const root = join(import.meta.dirname, "..");
const css = readFileSync(join(root, "shared/style.css"), "utf8");
const OMARCHY =
  ':root[data-platform="linux"][data-theme="system"][data-omarchy="true"]';

/** The declarations of the first rule whose selector is exactly `selector`. */
function block(source: string, selector: string) {
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
function declarations(body: string) {
  const out = new Map<string, string>();
  for (const m of body
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .matchAll(/([-a-z]+)\s*:\s*([^;]+);/g))
    out.set(m[1], m[2].replace(/\s+/g, " ").trim());
  return out;
}

type RGBA = [number, number, number, number];
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
function resolve(
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
    const part = (s: string) => {
      const m = /^(.*?)(?:\s+([\d.]+)%)?$/.exec(s)!;
      return {
        color: m[1],
        share: m[2] === undefined ? undefined : Number(m[2]) / 100,
      };
    };
    const [p, q] = [part(a), part(b)];
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
  if (v === "transparent") return [0, 0, 0, 0];
  const rgb = parseHex(v);
  if (rgb) return [...rgb, 1];
  return undefined;
}
/** `top` composited over the opaque `bottom`. */
const onto = (top: RGBA, bottom: RGB): RGB =>
  [0, 1, 2].map((i) => top[i] * top[3] + bottom[i] * (1 - top[3])) as RGB;
const opaque = (c: RGBA | undefined, what: string): RGB => {
  if (!c) throw new Error(`${what} does not resolve`);
  if (c[3] < 1) throw new Error(`${what} is not opaque`);
  return [c[0], c[1], c[2]];
};

/** The custom properties the root holds for an Omarchy theme: the default theme, the
 * Omarchy block over it, and the inline properties the app sets. */
function rootScope(properties: Record<string, string>) {
  const scope = new Map<string, string>();
  for (const [k, v] of declarations(block(css, ":root")))
    if (k.startsWith("--")) scope.set(k, v);
  for (const [k, v] of declarations(block(css, OMARCHY)))
    if (k.startsWith("--")) scope.set(k, v);
  for (const [k, v] of Object.entries(properties)) scope.set(k, v);
  return scope;
}

const SURFACES = [
  "bg",
  "panel",
  "raised",
  "overlay",
  "bubble",
  "hover",
  "active",
  "accent-soft",
  "warning-soft",
  "danger-soft",
  "loop-soft",
];
const BASES = ["bg", "panel", "raised", "overlay"];
const TEXT = [
  "text",
  "muted",
  "faint",
  "accent",
  "warning",
  "danger",
  "success",
  "loop",
];

interface Pair {
  what: string;
  ratio: number;
  min: number;
}
/** Every pair the app draws with the palette `properties`, and its contrast. */
function pairs(properties: Record<string, string>): Pair[] {
  const scope = rootScope(properties);
  const token = (name: string) =>
    opaque(resolve(`var(--${name})`, scope), name);
  const decl = (selector: string, property: string) => {
    const value = declarations(block(css, selector)).get(property);
    if (!value) throw new Error(`${selector} has no ${property}`);
    return value;
  };
  const out: Pair[] = [];
  const check = (what: string, fg: RGB, bg: RGB, min: number) =>
    out.push({ what, ratio: contrast(fg, bg), min });

  // Text in every colour, on every surface and tint it can sit on: the surfaces and soft
  // tints and a dived card for all; the "To:" chip in the composer (the orchestrator's
  // accent, a thread's colour) for the words beside the name; a success pill for success.
  const surfaces: [string, RGB][] = SURFACES.map((s) => [s, token(s)]);
  const chips: [string, RGB][] = [];
  const pills: [string, RGB][] = [];
  const orchestratorChip = resolve(
    decl(".destination-chip", "background"),
    new Map([...scope, ["--destination", "var(--accent)"]]),
  )!;
  chips.push(["accent 12% chip", onto(orchestratorChip, token("panel"))]);
  expect(decl(".composer", "background")).toBe("var(--panel)");
  for (const base of BASES)
    pills.push([
      `success pill on ${base}`,
      onto(resolve(decl(".pill.success", "background"), scope)!, token(base)),
    ]);
  for (const color of THREAD_COLORS) {
    const thread = new Map([
      ...scope,
      ["--thread-color", color],
      ["--destination", color],
    ]);
    surfaces.push([
      `${color} dived card`,
      opaque(
        resolve(decl(".thread-card.dived", "background"), thread),
        "dived card",
      ),
    ]);
    chips.push([
      `${color} 12% chip`,
      onto(
        resolve(decl(".destination-chip", "background"), thread)!,
        token("panel"),
      ),
    ]);
  }
  for (const fg of TEXT) {
    const on = [
      ...surfaces,
      ...(["text", "muted", "faint"].includes(fg) ? chips : []),
      ...(fg === "accent" ? chips.slice(0, 1) : []),
      ...(fg === "success" ? pills : []),
    ];
    for (const [where, bg] of on)
      check(`${fg} on ${where}`, token(fg), bg, TEXT_CONTRAST);
  }
  // Placeholders.
  expect(decl("input::placeholder,\ntextarea::placeholder", "color")).toBe(
    "var(--faint)",
  );

  // A filled button's label on its fill, resting and hovered; the microphone recording.
  const fill = (selector: string) =>
    opaque(resolve(decl(selector, "background"), scope), selector);
  check(
    "accent-fg on .primary",
    token("accent-fg"),
    fill(".primary"),
    TEXT_CONTRAST,
  );
  check(
    "accent-fg on .primary:hover",
    token("accent-fg"),
    fill(".primary:hover"),
    TEXT_CONTRAST,
  );
  check(
    "accent-fg on .send-button",
    token("accent-fg"),
    fill(".send-button"),
    TEXT_CONTRAST,
  );
  check(
    "accent-fg on a primary answer, hovered",
    token("accent-fg"),
    fill(".attention-options > button.primary:hover:not(:disabled)"),
    TEXT_CONTRAST,
  );
  check(
    "accent-fg on .voice-button.recording",
    token("accent-fg"),
    fill(".voice-button.recording"),
    TEXT_CONTRAST,
  );
  check(
    "danger-fg on .danger",
    token("danger-fg"),
    fill(".danger"),
    TEXT_CONTRAST,
  );
  check(
    "danger-fg on .danger:hover",
    token("danger-fg"),
    fill(".danger:hover"),
    TEXT_CONTRAST,
  );
  // Selected text.
  const selection = declarations(block(css, `${OMARCHY}\n  ::selection`));
  check(
    "selected text",
    opaque(resolve(selection.get("color")!, scope), "selection color"),
    opaque(resolve(selection.get("background")!, scope), "selection"),
    TEXT_CONTRAST,
  );

  // A field's edge and the composer's, and the focus ring, on everything around them.
  expect(decl("input,\ntextarea,\nselect", "border")).toBe(
    "1px solid var(--field-border)",
  );
  expect(decl(".composer", "border-color")).toBe("var(--border-strong)");
  for (const [on, bg] of surfaces.slice(0, 7)) {
    check(`field edge on ${on}`, token("field-border"), bg, EDGE_CONTRAST);
    check(`composer edge on ${on}`, token("border-strong"), bg, EDGE_CONTRAST);
  }
  for (const [on, bg] of surfaces)
    check(`focus ring on ${on}`, token("focus"), bg, EDGE_CONTRAST);

  // A thread's name in its colour's ink, on every background a name sits on; the "To:"
  // chip's conversation (a thread's ink, or the orchestrator's accent).
  const inkSurfaces = {
    bg: hexOf(token("bg")),
    panel: hexOf(token("panel")),
    raised: hexOf(token("raised")),
    overlay: hexOf(token("overlay")),
    hover: [token("hover"), 1] as [RGB, number],
    active: [token("active"), 1] as [RGB, number],
  };
  const inkRule = declarations(block(css, `${OMARCHY}\n  .thread-ink`)).get(
    "color",
  )!;
  const appRule = declarations(block(css, `${OMARCHY} .app`)).get(
    "--destination-ink",
  )!;
  for (const color of THREAD_COLORS) {
    const element = new Map(scope);
    for (const [k, v] of Object.entries(threadStyle(color)))
      element.set(k, String(v));
    const ink = opaque(resolve(inkRule, element), `${color} ink`);
    for (const [on, bg] of backgrounds(inkSurfaces, parseHex(color)!))
      check(`${color} name on ${on}`, ink, bg, TEXT_CONTRAST);
    const app = new Map([
      ...scope,
      ["--destination", color],
      ["--destination-ink-omarchy", omarchyInkVar(color)],
    ]);
    const chip = onto(
      resolve(decl(".destination-chip", "background"), app)!,
      token("panel"),
    );
    check(
      `${color} "To:" name`,
      opaque(resolve(appRule, app), "destination ink"),
      chip,
      TEXT_CONTRAST,
    );
  }
  const orchestrator = new Map([...scope, ["--destination", "var(--accent)"]]);
  check(
    'Orchestrator "To:"',
    opaque(resolve(appRule, orchestrator), "orchestrator ink"),
    onto(
      resolve(decl(".destination-chip", "background"), orchestrator)!,
      token("panel"),
    ),
    TEXT_CONTRAST,
  );
  return out;
}
const hexOf = (c: RGB) =>
  "#" + c.map((v) => Math.round(v).toString(16).padStart(2, "0")).join("");
const failing = (list: Pair[]) =>
  list
    .filter((p) => p.ratio < p.min)
    .map((p) => `${p.what}: ${p.ratio.toFixed(2)}:1`);

describe("Omarchy themes", () => {
  it("has every stock theme, light and dark", () => {
    const modes = Object.values(STOCK_THEMES).map(
      (t) => parseOmarchyColors(t)!.mode,
    );
    expect(Object.keys(STOCK_THEMES)).toHaveLength(23);
    expect(modes.filter((m) => m === "light")).toHaveLength(5);
    expect(modes.filter((m) => m === "dark")).toHaveLength(18);
  });

  for (const [name, toml] of Object.entries(STOCK_THEMES))
    it(`reads at 4.5:1 (3:1 for edges and focus) everywhere in ${name}`, () => {
      const theme = parseOmarchyColors(toml)!;
      const properties = omarchyProperties(theme)!;
      const list = pairs(properties);
      expect(list.length).toBeGreaterThan(1000);
      expect(failing(list)).toEqual([]);
    });

  it("keeps a theme's colour where it already reads, and its hue where it moves", () => {
    for (const toml of Object.values(STOCK_THEMES)) {
      const theme = parseOmarchyColors(toml)!;
      const { tokens } = omarchyPalette(theme)!;
      // The background is the theme's own, always.
      expect(tokens.bg).toBe(theme.background.toLowerCase());
      for (const [token, key] of [
        ["text", "foreground"],
        ["accent", "accent"],
        ["danger", "red"],
        ["warning", "yellow"],
      ] as const) {
        const given = parseHex(theme[key])!;
        const used = parseHex(tokens[token])!;
        if (tokens[token] === theme[key].toLowerCase()) continue;
        // Moved: same hue (OKLab), and lighter on a dark theme, darker on a light one.
        const lum = (c: RGB) => contrast(c, [0, 0, 0]);
        if (theme.mode === "dark")
          expect(lum(used), `${token}`).toBeGreaterThan(lum(given));
        else expect(lum(used), `${token}`).toBeLessThan(lum(given));
        const turn = hueTurn(given, used);
        if (turn !== undefined)
          expect(turn, `${theme.background} ${token}`).toBeLessThan(8);
      }
    }
  });

  it("leaves a palette whose pairs all read exactly as the theme gives it", () => {
    // hackerman's text, accent, warning and danger all read: none moves.
    const theme = parseOmarchyColors(STOCK_THEMES.hackerman)!;
    const { tokens } = omarchyPalette(theme)!;
    expect([tokens.text, tokens.accent, tokens.warning, tokens.danger]).toEqual(
      [theme.foreground, theme.accent, theme.yellow, theme.red].map((c) =>
        c.toLowerCase(),
      ),
    );
  });
});

describe("broken Omarchy themes", () => {
  const readable = (theme: Record<string, string>) => {
    const properties = omarchyProperties(theme);
    expect(properties).not.toBeNull();
    expect(failing(pairs(properties!))).toEqual([]);
    return properties!;
  };
  it("a foreground the same as the background", () => {
    readable({
      background: "#1e1e2e",
      foreground: "#1e1e2e",
      accent: "#1e1e2e",
      red: "#1e1e2e",
      yellow: "#1e1e2e",
    });
    readable({ mode: "light", background: "#fafafa", foreground: "#fafafa" });
  });
  it("a mid grey background, and a mode that contradicts it", () => {
    readable({ background: "#777777", foreground: "#888888" });
    readable({ mode: "light", background: "#101010", foreground: "#202020" });
  });
  it("missing keys", () => {
    readable({ background: "#282828", foreground: "#d4be98" });
    readable({ background: "#ffffff", foreground: "#000000" });
    expect(omarchyProperties({ background: "#282828" })).toBeNull();
    expect(omarchyProperties({ foreground: "#282828" })).toBeNull();
    expect(omarchyProperties({})).toBeNull();
    expect(omarchyProperties(null)).toBeNull();
  });
  it("invalid colour strings", () => {
    const props = readable({
      background: "#282828",
      foreground: "#d4be98",
      accent: "#zzzzzz",
      red: "red",
      yellow: "rgb(1, 2, 3)",
      selection: "#fff",
    });
    for (const value of Object.values(props))
      expect(value).toMatch(/^(#[0-9a-f]{6}|dark|light|\d+%)$/);
    expect(
      omarchyProperties({ background: "#28282", foreground: "#d4be98" }),
    ).toBeNull();
    expect(
      omarchyProperties({ background: "28282828", foreground: "#d4be98" }),
    ).toBeNull();
    expect(
      omarchyProperties({ background: 12 as never, foreground: "#d4be98" }),
    ).toBeNull();
  });
  it("reads colors.toml the way the app does", () => {
    expect(parseOmarchyColors("")).toBeNull();
    expect(parseOmarchyColors('background = "#000000"')).toBeNull();
    expect(
      parseOmarchyColors('background = "black"\nforeground = "#ffffff"'),
    ).toBeNull();
    expect(
      parseOmarchyColors(
        'mode = "light"\nbackground = \'#FFFFFF\'\nforeground = "#000000"\naccent = "#12345"\nred = "#ff0000" # comment',
      ),
    ).toEqual({
      mode: "light",
      background: "#FFFFFF",
      foreground: "#000000",
      red: "#ff0000",
    });
    const main = readFileSync(join(root, "desktop/main.ts"), "utf8");
    expect(main).toMatch(/return parseOmarchyColors\(\s*readFileSync\(/);
    const app = readFileSync(join(root, "shared/App.tsx"), "utf8");
    expect(app).toMatch(/const properties = omarchyProperties\(theme\);/);
  });
});

describe("the stylesheet draws only checked pairs", () => {
  // A new `color:` or `background:` with a token this test does not check fails here, so a
  // new pair cannot slip past the guarantee.
  const sheets = [
    "shared/style.css",
    "shared/attachments.css",
    "shared/inbox-search.css",
    "desktop/app-shell.css",
    "desktop/connections.css",
  ];
  const text = new Set([
    ...TEXT.map((t) => `var(--${t})`),
    "var(--accent-fg)",
    "var(--danger-fg)",
    "var(--destination-ink)",
    "var(--thread-ink-omarchy, var(--text))",
    "var(--accent-fg, var(--bg))",
    "var(--danger, #d66)",
    // The terminal keeps its own dark palette in every theme.
    "var(--terminal-text)",
    "var(--terminal-muted)",
    "inherit",
    "currentColor",
  ]);
  const fills = new Set([
    ...SURFACES.map((t) => `var(--${t})`),
    "none",
    "transparent",
    "var(--selection)",
    "var(--scrim)",
    "var(--terminal-bg)",
    "var(--terminal-panel)",
    // Fills that hold a label checked above (.primary, .danger, their hovers, recording) or
    // nothing to read (dots, meters, the resize grip).
    "var(--accent)",
    "var(--danger)",
    "var(--warning)",
    "var(--thread-color)",
    "var(--destination)",
    "color-mix(in srgb, var(--accent) 90%, var(--text))",
    "color-mix(in srgb, var(--danger) 88%, var(--text))",
    "color-mix(in srgb, var(--destination) 12%, transparent)",
    "color-mix(in srgb, var(--success) 12%, transparent)",
    "color-mix(in srgb, var(--thread-color) 8%, var(--panel))",
  ]);
  it("names a checked colour in every color and background", () => {
    const unknown: string[] = [];
    for (const sheet of sheets) {
      const source = readFileSync(join(root, sheet), "utf8").replace(
        /\/\*[\s\S]*?\*\//g,
        "",
      );
      for (const m of source.matchAll(
        /(?:^|[;{\s])(color|background|background-color)\s*:\s*([^;{}]+);/g,
      )) {
        const value = m[2]
          .replace(/\s+/g, " ")
          .replace(/\( /g, "(")
          .replace(/ \)/g, ")")
          .trim();
        if (
          value.startsWith("linear-gradient") ||
          value.includes("var(--terminal-")
        )
          continue;
        if (
          value ===
          "color-mix(in srgb, var(--thread-ink-light, var(--text)) var(--light-share), var(--thread-ink-dark, var(--text)))"
        )
          continue;
        const known = m[1] === "color" ? text : fills;
        if (!known.has(value)) unknown.push(`${sheet}: ${m[1]}: ${value}`);
      }
    }
    expect(unknown).toEqual([]);
  });
});

/** The OKLab hue turn (degrees) from `a` to `b`, undefined for a near-grey. */
function hueTurn(a: RGB, b: RGB) {
  const lab = (c: RGB) => {
    const lin = c.map((v) => {
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
    return [
      1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
      0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ];
  };
  const [x, y] = [lab(a), lab(b)];
  if (Math.hypot(...x) < 0.03) return undefined;
  const turn =
    ((Math.atan2(y[1], y[0]) - Math.atan2(x[1], x[0])) * 180) / Math.PI;
  return Math.abs(((turn + 540) % 360) - 180);
}
