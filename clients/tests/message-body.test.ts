import { describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MessageBody, streamingInterval } from "../shared/MessageBody";

vi.mock("../shared/host", () => ({
  useHostAPI: () => ({
    openExternal: async () => {},
    copyText: async () => {},
  }),
}));
const render = (
  text: string,
  options: {
    clamp?: boolean;
    mono?: boolean;
    expanded?: boolean;
    streaming?: boolean;
  } = {},
) => renderToStaticMarkup(createElement(MessageBody, { text, ...options }));

describe("long message rendering", () => {
  it("does not parse or mount a large hidden Markdown checklist while collapsed", () => {
    const source = Array.from(
      { length: 4000 },
      (_, n) =>
        `- Case ${n + 1}: Verify **pagination** and stable keyboard focus.`,
    ).join("\n");
    const html = render(source);
    expect(html.match(/<li>/g)).toHaveLength(12);
    expect(html.length).toBeLessThan(5000);
    expect(html).toContain("Case 1:");
    expect(html).not.toContain("Case 4000:");
    expect(html).toContain('aria-expanded="false"');
    expect(html).toContain("Show full message");
  });
  it("leaves complete non-collapsible assistant Markdown intact", () => {
    const source =
      Array.from(
        { length: 40 },
        (_, n) => `- Item ${n + 1}: **complete**`,
      ).join("\n") + "\n\nFinal paragraph 😀";
    const html = render(source, { clamp: false });
    expect(html.match(/<li>/g)).toHaveLength(40);
    expect(html).toContain("Item 40:");
    expect(html).toContain("Final paragraph 😀");
    expect(html).not.toContain("message-expand");
  });
  it("does not split a Unicode character at the preview boundary", () => {
    const source = "a".repeat(1599) + "😀" + "z".repeat(2000);
    const html = render(source);
    expect(html).not.toMatch(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])/);
    expect(html).not.toContain("\uFFFD");
    expect(html).toContain("Show full message");
    expect(render(source, { clamp: false })).toContain("😀");
  });
  it("bounds long tool text and escapes its markup", () => {
    const html = render("<script>not executable</script>".repeat(500), {
      mono: true,
    });
    expect(html.length).toBeLessThan(5000);
    expect(html).toContain("&lt;script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain('aria-expanded="false"');
  });
  it("renders an unfinished preview code fence as a complete safe code block", () => {
    const source =
      "```ts\n" +
      Array.from({ length: 40 }, () => 'const value = "<script>";').join("\n") +
      "\n```";
    const html = render(source);
    expect(html).toContain('class="language-ts"');
    expect(html).toContain("&lt;script&gt;");
    expect(html).toContain("</code></pre>");
    expect(html).not.toContain("<script>");
  });
  it("keeps a message expanded when the conversation says it was left open", () => {
    const source = Array.from({ length: 40 }, (_, n) => `line ${n + 1}`).join(
      "\n\n",
    );
    const html = render(source, { expanded: true });
    expect(html).toContain("line 40");
    expect(html).toContain('aria-expanded="true"');
    expect(html).toContain("Show less");
  });
  it("parses a long streaming reply less often than a short one", () => {
    expect(streamingInterval(0)).toBe(250);
    expect(streamingInterval(16384)).toBe(250);
    expect(streamingInterval(40 * 1024)).toBe(750);
    expect(streamingInterval(10 * 1024 * 1024)).toBe(1000);
    // Its first text is shown at once.
    expect(render("**hello**", { clamp: false, streaming: true })).toContain(
      "<strong>hello</strong>",
    );
  });
  it("names a web link's address and shows a path link as code to copy", () => {
    const html = render(
      "[docs](https://example.com/guide?q=1) and [App.tsx](clients/shared/App.tsx:42)",
      { clamp: false },
    );
    expect(html).toContain('title="https://example.com/guide?q=1"');
    expect(html).toContain(
      '<code title="clients/shared/App.tsx:42">App.tsx</code>',
    );
    expect(html).toContain('aria-label="Copy clients/shared/App.tsx:42"');
    expect(html).not.toContain('href="clients/shared/App.tsx:42"');
  });
});
