import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ts from "typescript";
import { describe, expect, it } from "vitest";
import { Dialog, FieldInput } from "../shared/ui";
import { MessageBody } from "../shared/MessageBody";

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
});

describe("styles that keep controls usable", () => {
  const css = readFileSync(join(root, "shared/style.css"), "utf8");
  const mobile = readFileSync(join(root, "mobile/src/mobile.css"), "utf8");
  const rule = (source: string, selector: string) => {
    const at = source.indexOf(selector + " {");
    return at < 0 ? "" : source.slice(at, source.indexOf("}", at));
  };
  it("keeps a primary request option's fill under the options' own background", () => {
    // `.attention-options > button` sets a raised background; a `.primary` option (a loop
    // setup card's Start and save) would keep its accent-fg label on it, unreadable.
    expect(rule(css, ".attention-options > button")).toContain("background");
    const primary = rule(css, ".attention-options > button.primary");
    expect(primary).toContain("background: var(--accent)");
    expect(primary).toContain("color: var(--accent-fg)");
  });
  it("gives the phone's disclosure rows and the model picker's list switch 44 points", () => {
    expect(rule(mobile, '[data-mobile="true"] details > summary')).toContain(
      "min-height: 44px",
    );
    expect(
      rule(mobile, '[data-mobile="true"] .input-with-button > button'),
    ).toContain("min-width: 44px");
    // Send as implementation loop's ↻ button and its choice, the inspector's two loop
    // starts and the phone's in-flow notice's Dismiss.
    expect(rule(mobile, '[data-mobile="true"] .send-loop-button')).toContain(
      "min-width: 44px",
    );
    expect(rule(mobile, '[data-mobile="true"] .send-loop-button')).toContain(
      "min-height: 44px",
    );
    const loopTargets = rule(
      mobile,
      '[data-mobile="true"] .send-loop-panel > button,\n[data-mobile="true"] .inspector-notice > button,\n[data-mobile="true"] .loop-starts > button',
    );
    expect(loopTargets).toContain("min-height: 44px");
    expect(loopTargets).toContain("min-width: 44px");
    // iOS ignores min-height on a menu select: its height must say 44.
    expect(
      rule(mobile, '[data-mobile="true"] select:not([multiple]):not([size])'),
    ).toContain("height: max(44px");
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
