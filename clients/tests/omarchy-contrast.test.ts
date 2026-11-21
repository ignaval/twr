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
import {
  block,
  controlPairs,
  declarations,
  onto,
  opaque,
  resolve,
} from "./css-contrast";

/* On Linux the desktop app draws with the selected Omarchy theme. This resolves the real
 * stylesheet (style.css's `:root` and Omarchy blocks, with the properties App.tsx puts on
 * the root for the theme) the way the browser does (tests/css-contrast.ts), and checks every
 * text and background pair the app draws, for every stock Omarchy theme and for broken ones:
 * 4.5:1 for text, 3:1 for every control's edge and state indicator and the focus ring (the
 * same list the light and dark themes are held to). */

const root = join(import.meta.dirname, "..");
const css = readFileSync(join(root, "shared/style.css"), "utf8");
const OMARCHY =
  ':root[data-platform="linux"][data-theme="system"][data-omarchy="true"]';

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
  // A message that starts with "!" runs in a shell: the composer's chip and the name in it
  // take the warning colour (`.composer.shell-mode` sets --destination to it).
  expect(decl(".composer.shell,\n.composer.shell-mode", "--destination")).toBe(
    "var(--warning)",
  );
  expect(
    decl(
      ".shell-mode .composer-label b,\n.shell-mode .destination-dot",
      "color",
    ),
  ).toBe("var(--warning)");
  const shellChip = onto(
    resolve(
      decl(".destination-chip", "background"),
      new Map([...scope, ["--destination", "var(--warning)"]]),
    )!,
    token("panel"),
  );
  chips.push(["warning 12% chip (shell mode)", shellChip]);
  check(
    'warning "To:" name on the shell-mode chip',
    token("warning"),
    shellChip,
    TEXT_CONTRAST,
  );
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
  expect(decl(".composer", "border-color")).toBe("var(--field-border)");
  for (const [on, bg] of surfaces.slice(0, 7))
    check(`field edge on ${on}`, token("field-border"), bg, EDGE_CONTRAST);
  // The composer's edge while it has the focus (it has no other focus ring), for the
  // orchestrator and each thread colour, and in shell mode.
  const appInk = declarations(block(css, `${OMARCHY} .app`)).get(
    "--destination-ink",
  )!;
  for (const color of ["orchestrator", ...THREAD_COLORS]) {
    const thread = color !== "orchestrator";
    const edge = opaque(
      resolve(
        decl(".composer:focus-within", "border-color"),
        new Map([
          ...scope,
          ["--destination", thread ? color : "var(--accent)"],
          ["--destination-ink", appInk],
          ...(thread
            ? [["--destination-ink-omarchy", omarchyInkVar(color)] as const]
            : []),
        ]),
      ),
      "focused composer edge",
    );
    for (const [on, bg] of surfaces.slice(0, 7))
      check(
        `focused composer edge (${color}) on ${on}`,
        edge,
        bg,
        EDGE_CONTRAST,
      );
  }
  expect(decl(".composer.shell-mode", "border-color")).toBe("var(--warning)");
  for (const [on, bg] of surfaces.slice(0, 7))
    check(
      `shell-mode composer edge on ${on}`,
      token("warning"),
      bg,
      EDGE_CONTRAST,
    );
  for (const [on, bg] of surfaces)
    check(`focus ring on ${on}`, token("focus"), bg, EDGE_CONTRAST);
  // Every control's edge and state indicator (tests/css-contrast.ts), with the properties
  // App.tsx and the Omarchy `.app` rule give a destination.
  const omarchyApp = [...declarations(block(css, `${OMARCHY} .app`))].filter(
    ([k]) => k.startsWith("--"),
  );
  out.push(
    ...controlPairs(scope, (color) => [
      ["--destination", color ?? "var(--accent)"],
      ...(color
        ? ([["--destination-ink-omarchy", omarchyInkVar(color)]] as [
            string,
            string,
          ][])
        : []),
      ...omarchyApp,
    ]),
  );

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
    const reader = readFileSync(join(root, "desktop/omarchy-theme.ts"), "utf8");
    expect(reader).toMatch(
      /return parseOmarchyColors\(readFileSync\(omarchyColorsFile\(home\)/,
    );
    const main = readFileSync(join(root, "desktop/main.ts"), "utf8");
    expect(main).toMatch(/readOmarchyTheme\(homedir\(\)\)/);
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
    // nothing to read (dots, meters, the resize grip, a slider's track, a switch's thumb,
    // tests/css-contrast.ts).
    "var(--accent)",
    "var(--accent-fg)",
    "var(--field-border)",
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
  it("draws every field's edge in a checked colour", () => {
    // A field's own rule may not put back an edge the palette does not hold to 3:1 (the
    // rule for every field uses --field-border; focus moves it to the accent).
    const edges = new Set([
      "var(--field-border)",
      "var(--accent)",
      "var(--focus)",
      "0",
      "none",
      // The terminal keeps its own dark palette in every theme; its search field's edge is
      // held to 3:1 against it (tests/css-contrast.ts).
      "var(--terminal-border)",
      "var(--terminal-field-border)",
    ]);
    const field =
      /(^|[\s,>+~(])(input|textarea|select)(?![-\w])(?!\[type="(checkbox|radio|range)"\])/;
    const unknown: string[] = [];
    for (const sheet of sheets) {
      const source = readFileSync(join(root, sheet), "utf8").replace(
        /\/\*[\s\S]*?\*\//g,
        "",
      );
      for (const rule of source.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
        const selector = rule[1].replace(/\s+/g, " ").trim();
        if (!field.test(selector)) continue;
        for (const m of rule[2].matchAll(
          /(border(?:-(?:top|right|bottom|left))?(?:-color)?)\s*:\s*([^;]+);/g,
        )) {
          const value = m[2].replace(/\s+/g, " ").trim();
          const color = /var\([^)]*\)\)?$/.exec(value)?.[0] ?? value;
          if (!edges.has(color))
            unknown.push(`${sheet}: ${selector}: ${m[1]}: ${value}`);
        }
      }
    }
    expect(unknown).toEqual([]);
  });

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
