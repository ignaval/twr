import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ts from "typescript";
import { describe, expect, it } from "vitest";
import { Dialog, FieldInput } from "../shared/ui";
import { MessageBody } from "../shared/MessageBody";
import { controlPairs, failing, themeScope } from "./css-contrast";

/* A dependency-free accessibility lint over the shared React UI: it parses the
 * TSX with the TypeScript compiler already used by the build, and checks theme
 * tokens for WCAG AA contrast. It cannot replace a screen-reader pass, but it
 * keeps unnamed icon buttons, unlabelled fields and low-contrast tokens out. */

const root = join(import.meta.dirname, "..");
const sources = [
  ...readdirSync(join(root, "shared"))
    .filter((name) => name.endsWith(".tsx"))
    .map((name) => join("shared", name)),
  "desktop/Connections.tsx",
  "desktop/renderer.tsx",
];

const namingAttributes = ["aria-label", "aria-labelledby"];
function attributes(node: ts.JsxOpeningLikeElement) {
  const names = new Map<string, ts.JsxAttribute>();
  for (const property of node.attributes.properties)
    if (ts.isJsxAttribute(property))
      names.set(property.name.getText(), property);
  return names;
}
function tagOf(node: ts.JsxOpeningLikeElement) {
  return node.tagName.getText();
}
function iconImports(file: ts.SourceFile) {
  const icons = new Set(["Spinner"]);
  for (const statement of file.statements)
    if (
      ts.isImportDeclaration(statement) &&
      (statement.moduleSpecifier as ts.StringLiteral).text === "lucide-react"
    )
      for (const element of (
        statement.importClause?.namedBindings as ts.NamedImports | undefined
      )?.elements || [])
        icons.add(element.name.text);
  return icons;
}
/** True when a child can contribute readable text to an accessible name. */
function hasTextContent(
  children: ts.NodeArray<ts.JsxChild>,
  icons: Set<string>,
): boolean {
  return children.some((child) => {
    if (ts.isJsxText(child)) return child.text.trim().length > 0;
    if (ts.isJsxExpression(child)) return !!child.expression;
    if (ts.isJsxSelfClosingElement(child))
      return !icons.has(tagOf(child)) && tagOf(child) !== "span";
    if (ts.isJsxElement(child)) {
      const opening = child.openingElement;
      if (attributes(opening).get("aria-hidden")) return false;
      return hasTextContent(child.children, icons);
    }
    if (ts.isJsxFragment(child)) return hasTextContent(child.children, icons);
    return false;
  });
}
function insideLabel(node: ts.Node) {
  for (let parent = node.parent; parent; parent = parent.parent)
    if (ts.isJsxElement(parent) && tagOf(parent.openingElement) === "label")
      return true;
  return false;
}

function audit(path: string) {
  const text = readFileSync(join(root, path), "utf8");
  const file = ts.createSourceFile(
    path,
    text,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  );
  const icons = iconImports(file);
  const problems: string[] = [];
  const where = (node: ts.Node) =>
    `${path}:${file.getLineAndCharacterOfPosition(node.getStart()).line + 1}`;
  const visit = (node: ts.Node) => {
    const opening = ts.isJsxElement(node)
      ? node.openingElement
      : ts.isJsxSelfClosingElement(node)
        ? node
        : undefined;
    if (opening) {
      const tag = tagOf(opening);
      const attrs = attributes(opening);
      const named = namingAttributes.some((name) => attrs.has(name));
      if (
        (tag === "button" || tag === "summary") &&
        !named &&
        !(ts.isJsxElement(node) && hasTextContent(node.children, icons))
      )
        problems.push(`${where(node)} <${tag}> has no accessible name`);
      if (["input", "textarea", "select"].includes(tag)) {
        const type = attrs.get("type")?.initializer?.getText();
        if (
          type !== '"hidden"' &&
          !named &&
          !attrs.has("id") &&
          !insideLabel(node)
        )
          problems.push(`${where(node)} <${tag}> has no label`);
      }
      if (tag === "img" && !attrs.has("alt"))
        problems.push(`${where(node)} <img> has no alt text`);
      if (
        (tag === "div" || tag === "span" || tag === "article") &&
        attrs.has("onClick") &&
        !attrs.has("role") &&
        !attrs.has("tabIndex") &&
        // Event delegation containers that only close menus or sheets.
        !attrs.has("onKeyDown")
      ) {
        const handler = attrs.get("onClick")!.getText();
        if (!/removeAttribute\("open"\)/.test(handler))
          problems.push(
            `${where(node)} <${tag}> is clickable but not focusable`,
          );
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(file);
  return problems;
}

describe("accessibility audit of the shared UI", () => {
  it("names every button, labels every field and describes every image", () => {
    expect(sources.flatMap(audit)).toEqual([]);
  });

  // A label that holds a hint (or a button) as well as its field's name would name the
  // field with both ("Sweep The thread audits each rule itself…", "Project or repository
  // Choose folder"): the field is named by its own word, and the hint describes it.
  it("names a field by its label's word, not by the hint or button the label holds", () => {
    const labelled = (path: string) => {
      const text = readFileSync(join(root, path), "utf8");
      const file = ts.createSourceFile(
        path,
        text,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.TSX,
      );
      const problems: string[] = [];
      const visit = (node: ts.Node) => {
        if (ts.isJsxElement(node) && tagOf(node.openingElement) === "label") {
          const inside: ts.JsxOpeningLikeElement[] = [];
          const collect = (n: ts.Node) => {
            if (ts.isJsxElement(n)) inside.push(n.openingElement);
            else if (ts.isJsxSelfClosingElement(n)) inside.push(n);
            ts.forEachChild(n, collect);
          };
          node.children.forEach(collect);
          const extra = inside.some((e) =>
            ["small", "button"].includes(tagOf(e)),
          );
          for (const field of inside.filter((e) =>
            ["input", "textarea", "select"].includes(tagOf(e)),
          ))
            if (
              extra &&
              !namingAttributes.some((name) => attributes(field).has(name))
            )
              problems.push(
                `${path}:${file.getLineAndCharacterOfPosition(field.getStart()).line + 1} <${tagOf(field)}> is named by its label's hint or button too`,
              );
        }
        ts.forEachChild(node, visit);
      };
      visit(file);
      return problems;
    };
    expect(sources.flatMap(labelled)).toEqual([]);
  });

  it("names a form's field by its label alone, described by its description", () => {
    const html = renderToStaticMarkup(
      createElement(FieldInput, {
        field: {
          key: "project",
          label: "Project or repository",
          type: "directory",
          description: "A folder, or a repository to clone.",
        },
        value: "",
        onChange() {},
      }),
    );
    // The label holds the name alone: the folder button and the description are outside.
    const label = html.match(/<label[^>]*>(.*?)<\/label>/)?.[1] ?? "";
    expect(label).toBe("Project or repository");
    expect(html).toContain('for="field-project"');
    expect(html).toMatch(
      /<input[^>]*id="field-project"[^>]*aria-describedby="field-project-description"/,
    );
    expect(html).toContain(
      '<small id="field-project-description">A folder, or a repository to clone.</small>',
    );
    expect(html).toContain('aria-label="Choose folder"');
  });

  it("keeps a primary answer's accent background over the answers' raised one", () => {
    // `.attention-options > button` is more specific than `.primary`: without its own rule
    // a loop's Start and save had the accent's foreground on the raised background (white
    // on white in the light theme).
    const css = readFileSync(join(root, "shared/style.css"), "utf8");
    const rule = /\.attention-options > button\.primary \{([^}]*)\}/.exec(css);
    expect(rule?.[1]).toMatch(/background: var\(--accent\);/);
    expect(rule?.[1]).toMatch(/color: var\(--accent-fg\);/);
  });

  it("keeps a visible keyboard focus style and honours reduced motion", () => {
    const css = readFileSync(join(root, "shared/style.css"), "utf8");
    expect(css).toMatch(/(^|\n):focus-visible \{\s*outline: 2px solid/);
    expect(css).toMatch(/input\[type="checkbox"\]:focus-visible/);
    expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\)/);
    // Any outline removal must be paired with a replacement indicator.
    for (const match of css.matchAll(/([^{}]+)\{[^{}]*outline: (none|0)/g)) {
      const selector = match[1].trim();
      expect(
        selector.includes(":focus-visible") ||
          /^input,\s*textarea,\s*select$/.test(selector),
        selector,
      ).toBe(true);
    }
  });

  it("gives a keyboard jump's message target an outline above tabindex=-1 suppression", () => {
    const css = readFileSync(join(root, "shared/style.css"), "utf8");
    // The general suppression has specificity (0, 2, 1). The message rule's
    // class, attribute and pseudo-class give it (0, 3, 0), so it wins the cascade.
    const rule =
      /\.transcript-block\[tabindex="-1"\]:focus-visible\s*\{([^}]*)\}/.exec(
        css,
      );
    expect(rule?.[1]).toMatch(/outline:\s*2px solid var\(--focus\)/);
    expect(rule?.[1]).toMatch(/outline-offset:\s*-2px/);
  });
});

const hex = (value: string) =>
  [1, 3, 5].map((i) => parseInt(value.slice(i, i + 2), 16));
const luminance = (rgb: number[]) =>
  rgb
    .map((v) => v / 255)
    .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
    .reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
const contrast = (a: string, b: string) => {
  const [high, low] = [luminance(hex(a)), luminance(hex(b))].sort(
    (x, y) => y - x,
  );
  return (high + 0.05) / (low + 0.05);
};
function tokens(block: string) {
  return Object.fromEntries(
    [...block.matchAll(/--([a-z-]+):\s*(#[0-9a-f]{6});/gi)].map((m) => [
      m[1],
      m[2],
    ]),
  );
}

describe("theme contrast", () => {
  const css = readFileSync(join(root, "shared/style.css"), "utf8");
  const dark = tokens(css.slice(0, css.indexOf("}")));
  const lightStart = css.indexOf(':root[data-theme="light"] {');
  const light = tokens(css.slice(lightStart, css.indexOf("}", lightStart)));
  const systemStart = css.indexOf(':root[data-theme="system"] {');
  const system = tokens(css.slice(systemStart, css.indexOf("}", systemStart)));

  it("keeps the system light theme identical to the explicit light theme", () => {
    expect(system).toEqual(light);
  });

  for (const [name, theme] of [
    ["dark", dark],
    ["light", light],
  ] as const)
    it(`meets WCAG AA (4.5:1) for text tokens in the ${name} theme`, () => {
      const failures: string[] = [];
      for (const fg of [
        "text",
        "muted",
        "faint",
        "accent",
        "warning",
        "danger",
        "success",
        "loop",
      ])
        for (const bg of ["bg", "panel", "raised", "overlay"]) {
          const ratio = contrast(theme[fg], theme[bg]);
          if (ratio < 4.5)
            failures.push(`${fg} on ${bg}: ${ratio.toFixed(2)}:1`);
        }
      expect(contrast(theme["accent-fg"], theme.accent)).toBeGreaterThan(4.5);
      expect(contrast(theme["danger-fg"], theme.danger)).toBeGreaterThan(4.5);
      expect(failures).toEqual([]);
    });

  // Warning text on the tints it sits on: Needs you's count (warning-soft) on the selected
  // sidebar row (accent-soft over the panel), and the composer's "To:" chip in shell mode
  // (warning at 12% over the panel). The app's own palettes are what an unusable Omarchy
  // theme falls back to; both pairs were under 4.5:1 in the light one (4.01:1, 4.48:1).
  const tints = (start: number) => {
    const block = css.slice(start, css.indexOf("}", start));
    return Object.fromEntries(
      [
        ...block.matchAll(
          /--([a-z-]+):\s*rgb\((\d+) (\d+) (\d+) \/ ([\d.]+)%\);/g,
        ),
      ].map((m) => [m[1], { rgb: [+m[2], +m[3], +m[4]], alpha: +m[5] / 100 }]),
    );
  };
  const over = (top: number[], alpha: number, bottom: number[]) =>
    top.map((v, i) => v * alpha + bottom[i] * (1 - alpha));
  const toHex = (rgb: number[]) =>
    "#" + rgb.map((v) => Math.round(v).toString(16).padStart(2, "0")).join("");
  for (const [name, theme, start] of [
    ["dark", dark, 0],
    ["light", light, lightStart],
  ] as const)
    it(`reads warning text on its tints in the ${name} theme`, () => {
      const soft = tints(start);
      const panel = hex(theme.panel);
      const row = over(
        soft["accent-soft"].rgb,
        soft["accent-soft"].alpha,
        panel,
      );
      const count = over(
        soft["warning-soft"].rgb,
        soft["warning-soft"].alpha,
        row,
      );
      const chip = over(hex(theme.warning), 0.12, panel);
      expect(
        contrast(theme.warning, toHex(count)),
        "the Needs you count on the selected row",
      ).toBeGreaterThanOrEqual(4.5);
      expect(
        contrast(theme.warning, toHex(chip)),
        "the shell-mode chip",
      ).toBeGreaterThanOrEqual(4.5);
    });
});

describe("control contrast", () => {
  // Every control's edge and state indicator (WCAG 2.2 1.4.11), resolved from the real
  // stylesheets: fields, the composer, checkboxes and switches, sliders, the selected
  // segment, tab, thread card and list item, pressed toggles and the focus ring, on every
  // surface and tint they sit on (tests/css-contrast.ts).
  for (const theme of ["dark", "light"] as const)
    it(`reads at 3:1 for every control edge and state in the ${theme} theme`, () => {
      const list = controlPairs(themeScope(theme));
      expect(list.length).toBeGreaterThan(80);
      expect(failing(list)).toEqual([]);
    });
});

describe("styles that keep controls usable", () => {
  const css = readFileSync(join(root, "shared/style.css"), "utf8");
  const mobile = readFileSync(join(root, "mobile/src/mobile.css"), "utf8");
  const rule = (source: string, selector: string) => {
    const at = source.indexOf(selector + " {");
    return at < 0 ? "" : source.slice(at, source.indexOf("}", at));
  };
  it("keeps a loop report's text in sight at the largest text size", () => {
    // The title and the path do not shrink: the text (the one part that scrolls) keeps a
    // floor, and a long path scrolls inside the footer.
    expect(rule(css, ".loop-report-body")).toContain(
      "min-height: min(10rem, 30dvh)",
    );
    const footer = rule(css, ".loop-report-footer");
    expect(footer).toContain("max-height: 22dvh");
    expect(footer).toContain("overflow-y: auto");
  });
  it("never lets a pane's scrollIntoView scroll the layout boxes that clip", () => {
    // At 1024 × 674 the inspector's Loops section, brought into view, scrolled
    // `.work-area` by 56 px: a visually hidden label in the inspector's body, placed by
    // `.inspector`, overflowed it. A clipping box can't be scrolled; the body places its
    // own labels.
    for (const box of [".work-area", ".content-column"]) {
      expect(rule(css, box)).toContain("overflow: clip");
      expect(rule(css, box)).not.toContain("overflow: hidden");
    }
    expect(rule(css, ".inspector-body")).toContain("position: relative");
  });
  it("cues a request that goes on below the conversation's requests area", () => {
    const area = rule(css, ".conversation-attention");
    expect(area).toContain("max-height: 35%");
    expect(area).toContain("overflow: auto");
    const cue = rule(css, '.conversation-attention[data-more="below"]::after');
    expect(cue).toContain("position: sticky");
    expect(cue).toContain("pointer-events: none");
    const app = readFileSync(join(root, "shared/App.tsx"), "utf8");
    expect(app).toMatch(
      /className="conversation-attention"\s+ref=\{scrollCue\}/,
    );
  });
  it("mounts the loop dialogs opened in the inspector in the work area", () => {
    const views = readFileSync(join(root, "shared/LoopViews.tsx"), "utf8");
    for (const dialog of ["<LoopReportDialog", "<ImplementTaskDialog"])
      expect(views).toMatch(new RegExp(`<AppLayer>\\s*${dialog}`));
    expect(views).toMatch(/<AppLayer>\s*<Dialog\s+title=\{`\$\{heading\}/);
  });
  it("lets a card's long status give way before the thread's name", () => {
    // `sweeper`'s card read `s… background (1 bg)` while its loop's reviewer ran.
    const name = rule(css, ".thread-card-title > b");
    expect(name).toContain("flex-shrink: 0");
    expect(name).toContain("max-width: 65%");
    const status = rule(css, ".thread-card-title > .thread-status");
    expect(status).toContain("min-width: 0");
    expect(status).toContain("overflow: hidden");
    expect(
      rule(css, ".thread-card-title > .thread-status > .thread-status-label"),
    ).toContain("text-overflow: ellipsis");
    const app = readFileSync(join(root, "shared/App.tsx"), "utf8");
    expect(app).toMatch(
      /<span className="thread-status-label">\s*\{thread\.status_label \|\| thread\.status\}/,
    );
  });
  it("keeps a primary request option's fill under the options' own background", () => {
    // `.attention-options > button` sets a raised background; a `.primary` option (a loop
    // setup card's Start and save) would keep its accent-fg label on it, unreadable.
    expect(rule(css, ".attention-options > button")).toContain("background");
    const primary = rule(css, ".attention-options > button.primary");
    expect(primary).toContain("background: var(--accent)");
    expect(primary).toContain("color: var(--accent-fg)");
  });
  it("draws a switch as a track and thumb, 44 points to touch on the phone", () => {
    const sw = 'input[type="checkbox"][role="switch"]';
    expect(rule(css, sw)).toContain("border-radius: 999px");
    expect(rule(css, `${sw}:checked`)).toContain("background: var(--accent)");
    expect(rule(css, `${sw}:checked::before`)).toContain(
      "transform: translateX",
    );
    // The input itself is the 44-point target (what is measured and tapped); its ::after
    // draws the 26-high track inside it, and turns the accent when on.
    const target = rule(mobile, `[data-mobile="true"] ${sw}:checked`);
    expect(target).toContain("min-width: 44px");
    expect(target).toContain("min-height: 44px");
    const track = rule(mobile, `[data-mobile="true"] ${sw}::after`);
    expect(track).toContain("height: 26px");
    expect(track).toContain("border-radius: 999px");
    expect(rule(mobile, `[data-mobile="true"] ${sw}:checked::after`)).toContain(
      "background: var(--accent)",
    );
  });
  it("gives the phone's disclosure rows and the model picker's list switch 44 points", () => {
    expect(rule(mobile, '[data-mobile="true"] details > summary')).toContain(
      "min-height: 44px",
    );
    expect(
      rule(mobile, '[data-mobile="true"] .input-with-button > button'),
    ).toContain("min-width: 44px");
    // Run as loop's ↻ switch, the inspector's two loop starts and the phone's in-flow
    // notice's Dismiss.
    expect(rule(mobile, '[data-mobile="true"] .send-loop-button')).toContain(
      "min-width: 44px",
    );
    expect(rule(mobile, '[data-mobile="true"] .send-loop-button')).toContain(
      "min-height: 44px",
    );
    const loopTargets = rule(
      mobile,
      '[data-mobile="true"] .inspector-notice > button,\n[data-mobile="true"] .loop-starts > button',
    );
    expect(loopTargets).toContain("min-height: 44px");
    expect(loopTargets).toContain("min-width: 44px");
    // Every control in the composer's row: Upload, the microphone, hands-free, Send (and
    // Run as loop, above). The microphone and hands-free were 32 wide.
    const composerTargets = rule(
      mobile,
      '[data-mobile="true"] .send-button,\n[data-mobile="true"] .voice-button',
    );
    for (const size of [
      "min-width: 44px",
      "min-height: 44px",
      "width: 44px",
      "height: 44px",
    ])
      expect(composerTargets).toContain(size);
    expect(rule(mobile, ".attachment-button")).toContain("min-width: 44px");
    expect(rule(mobile, ".attachment-button")).toContain("min-height: 44px");
    // 5 × 44 points and their gaps do not fit beside the message on a 360-point phone:
    // in portrait (and a fold's narrow pane) they get their own row under it, at full size.
    const narrowAt = mobile.indexOf("@media (max-width: 519px) {");
    expect(narrowAt).toBeGreaterThan(0);
    const narrow = mobile.slice(narrowAt, mobile.indexOf("\n}\n", narrowAt));
    expect(narrow).toMatch(
      /\.composer > textarea \{\s*grid-column: 1 \/ -1;\s*\}/,
    );
    expect(narrow).toMatch(
      /\.composer-bottom \{\s*grid-column: 1 \/ -1;\s*grid-row: 3;/,
    );
    expect(
      rule(
        mobile,
        '[data-mobile="true"][data-fold="vertical"] .composer-bottom',
      ),
    ).toContain("grid-row: 3");
    expect(narrow).not.toMatch(/(width|height): (3\d|2\d)px/);
    // The desktop keeps its compact 32-pixel buttons.
    expect(rule(css, ".voice-button")).toContain("width: 32px");
    // iOS ignores min-height on a menu select: its height must say 44.
    expect(
      rule(mobile, '[data-mobile="true"] select:not([multiple]):not([size])'),
    ).toContain("height: max(44px");
  });
  it("gives the phone's attached files 44-point targets inside the message box", () => {
    const remove = rule(mobile, '[data-mobile="true"] .attachment-remove');
    for (const size of [
      "min-width: 44px",
      "min-height: 44px",
      "width: 44px",
      "height: 44px",
    ])
      expect(remove).toContain(size);
    expect(
      rule(mobile, '[data-mobile="true"] .attachment-chip > .attachment-open'),
    ).toContain("min-height: 44px");
    // The files take the box's width, above the text, and wrap rather than overflow.
    expect(
      rule(mobile, '[data-mobile="true"] .composer-attachments'),
    ).toContain("grid-column: 1 / -1");
    const attachments = readFileSync(
      join(root, "shared/attachments.css"),
      "utf8",
    );
    expect(attachments).toMatch(
      /\.composer-attachments > ul \{[^}]*flex-wrap: wrap;/,
    );
    expect(attachments).toMatch(/\.attachment-tile \{[^}]*width: 64px;/);
  });
});
describe("loop surfaces", () => {
  const css = readFileSync(join(root, "shared/style.css"), "utf8");
  it("lets a long held task scroll inside the setup card, and be scrolled by keyboard", () => {
    const at = css.indexOf(".loop-setup-task {");
    const rule = css.slice(at, css.indexOf("}", at));
    expect(rule).toContain("overflow: auto");
    expect(rule).toContain("max-height");
    expect(css).toContain(".loop-setup-task:focus-visible");
  });
  it("marks an uncommitted end with an icon and words, not colour alone", () => {
    const views = readFileSync(join(root, "shared/LoopViews.tsx"), "utf8");
    const at = views.indexOf("export function LoopEndedLine");
    const body = views.slice(at, views.indexOf("\n}\n", at));
    expect(body).toContain("<AlertCircle");
    expect(body).toContain("ended: ");
  });
});

describe("rendered markup", () => {
  (globalThis as any).window ??= { twr: {} };
  it("labels dialogs by their title and describes them by their subtitle", () => {
    const html = renderToStaticMarkup(
      createElement(Dialog, {
        title: "Close terminal?",
        subtitle: "It will exit.",
        onClose() {},
        children: "body",
      }),
    );
    const labelledBy = html.match(/aria-labelledby="([^"]+)"/)?.[1];
    const describedBy = html.match(/aria-describedby="([^"]+)"/)?.[1];
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
    expect(html).toContain(`<h2 id="${labelledBy}">Close terminal?</h2>`);
    expect(html).toContain(`<p id="${describedBy}">It will exit.</p>`);
    expect(html).toContain('aria-label="Close dialog"');
  });

  it("marks required fields programmatically, not only with an asterisk", () => {
    const html = renderToStaticMarkup(
      createElement(FieldInput, {
        field: { key: "name", label: "Name", required: true },
        value: "",
        onChange() {},
      }),
    );
    expect(html).toContain('aria-required="true"');
    expect(html).toMatch(/<span class="required" aria-hidden="true">/);
    expect(html).toContain('for="field-name"');
    expect(html).toContain('id="field-name"');
  });

  it("connects the long-message toggle to the text it expands", () => {
    const html = renderToStaticMarkup(
      createElement(MessageBody, { text: "line\n".repeat(40), mono: true }),
    );
    const controls = html.match(/aria-controls="([^"]+)"/)?.[1];
    expect(controls).toBeTruthy();
    expect(html).toContain(`id="${controls}"`);
    expect(html).toContain('aria-expanded="false"');
  });
});
