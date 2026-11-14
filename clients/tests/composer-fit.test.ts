import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { fitToText } from "../shared/Composer";

/* The message field grows with its text, wrapped lines included, at every text size: the
 * browser reports the wrapped text's height (scrollHeight), and fitToText makes the field
 * that tall. CSS keeps it between its min-height and max-height, past which it scrolls. */

const root = join(import.meta.dirname, "..");

/** A stand-in field: `lines` of text at `line` pixels each, in a field `rows` tall. */
function field({
  lines,
  rows = 2,
  line = 20,
  padding = 20,
  border = 0,
}: {
  lines: number;
  rows?: number;
  line?: number;
  padding?: number;
  border?: number;
}) {
  const box = {
    style: { height: "" },
    held: [] as string[],
    getBoundingClientRect: () => ({ height: 180.5 }),
  };
  const style = { height: "" };
  const own = () =>
    style.height ? parseFloat(style.height) : rows * line + padding + border;
  const element = {
    isConnected: true,
    parentElement: box,
    style,
    get clientHeight() {
      return own() - border;
    },
    get offsetHeight() {
      // The composer's box holds its height while the field is measured.
      box.held.push(box.style.height);
      return own();
    },
    get scrollHeight() {
      return Math.max(lines * line + padding, own() - border);
    },
  };
  return { element, box };
}

describe("the message field's height", () => {
  it("grows to the height of its wrapped text", () => {
    const { element, box } = field({ lines: 6 });
    fitToText(element as never);
    expect(element.style.height).toBe("140px");
    // Held at its own height while measured, and given back after.
    expect(box.held).toContain("180.5px");
    expect(box.style.height).toBe("");
  });
  it("counts the field's borders on top of its text", () => {
    const { element } = field({ lines: 5, border: 2 });
    fitToText(element as never);
    expect(element.style.height).toBe("122px");
  });
  it("shrinks back to its rows when the text goes (sent, or deleted)", () => {
    const short = field({ lines: 1 });
    short.element.style.height = "140px";
    fitToText(short.element as never);
    expect(short.element.style.height).toBe("");
  });
  it("leaves a field that is not on screen alone", () => {
    const { element } = field({ lines: 6 });
    element.isConnected = false;
    fitToText(element as never);
    expect(element.style.height).toBe("");
    expect(() => fitToText(null)).not.toThrow();
  });
});

describe("where the message field stops growing", () => {
  const css = readFileSync(join(root, "shared/style.css"), "utf8");
  const mobile = readFileSync(join(root, "mobile/src/mobile.css"), "utf8");
  const composer = readFileSync(join(root, "shared/Composer.tsx"), "utf8");
  it("measures on every change of the text, the width and the text size", () => {
    expect(composer).toContain(
      "useLayoutEffect(() => fitToText(input.current), [draft, input]);",
    );
    expect(composer).toContain("new ResizeObserver(");
    expect(composer).toContain(
      'window.addEventListener("twr:typography", refit);',
    );
    // The rows are the empty field's height, not a count of the text's line breaks.
    expect(composer).not.toMatch(/draft\.split\("\\n"\)\.length/);
  });
  it("caps it at 240 px or 40% of the window on a computer", () => {
    const at = css.indexOf(".composer > textarea {");
    const rule = css.slice(at, css.indexOf("}", at));
    expect(rule).toContain("max-height: min(240px, 40vh)");
    expect(rule).toContain("resize: none");
  });
  it("caps it at four lines (three on a very short screen) and 40% of what the keyboard leaves on a phone", () => {
    // The last rules for the field win over the earlier max-heights.
    const last = mobile.lastIndexOf(
      '[data-mobile="true"] .composer > textarea {',
    );
    const rule = mobile.slice(last, mobile.indexOf("}", last));
    expect(rule).toMatch(
      /max-height: min\(\s*calc\(var\(--viewport-height, 100dvh\) \* 0\.4\),\s*calc\(4lh \+ 20px\)\s*\)/,
    );
    const tinyAt = mobile.lastIndexOf(
      ':root[data-mobile="true"].tiny-content .composer > textarea {',
    );
    expect(mobile.slice(tinyAt, mobile.indexOf("}", tinyAt))).toMatch(
      /max-height: min\(\s*calc\(var\(--viewport-height, 100dvh\) \* 0\.4\),\s*calc\(3lh \+ 20px\)\s*\)/,
    );
    expect(rule).not.toContain("40dvh");
  });
});
