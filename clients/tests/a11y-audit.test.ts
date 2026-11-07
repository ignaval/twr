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
      ])
        for (const bg of ["bg", "panel", "raised", "overlay"]) {
          const ratio = contrast(theme[fg], theme[bg]);
          if (ratio < 4.5)
            failures.push(`${fg} on ${bg}: ${ratio.toFixed(2)}:1`);
        }
      expect(contrast(theme["accent-fg"], theme.accent)).toBeGreaterThan(4.5);
      expect(failures).toEqual([]);
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
