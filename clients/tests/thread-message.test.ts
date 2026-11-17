import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { MessageBody } from "../shared/MessageBody";
import { threadSections } from "../shared/ThreadMessage";

vi.mock("../shared/host", () => ({
  useHostAPI: () => ({
    openExternal: async () => {},
    copyText: async () => {},
  }),
}));

const render = (
  text: string,
  options: { threadTags?: boolean; streaming?: boolean; mono?: boolean } = {},
) =>
  renderToStaticMarkup(
    createElement(MessageBody, {
      text,
      clamp: false,
      threadTags: true,
      ...options,
    }),
  );

describe("orchestrator thread sections", () => {
  it("renders the reported reply as a named box followed by ordinary prose", () => {
    const html = render(
      '<thread name="install-topbar-fix">\nStarted the merge, local installation, and restart using Codex GPT-6.1 Sol on high.\n</thread>\nOnce verified, I’ll prepare the cleanup card.',
    );
    expect(html).toContain(
      'class="thread-message" aria-label="install-topbar-fix"',
    );
    expect(html).toContain(
      '<div class="thread-message-name">install-topbar-fix</div>',
    );
    expect(html).toContain("Started the merge");
    expect(html).toContain('</section><div class="markdown"><p>Once verified');
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
  });

  it("keeps prose and multiple sections in order, including unknown names and CRLF", () => {
    expect(
      threadSections(
        'Before\r\n  <thread name=" alpha ">  \r\n**one**\r\n </thread> \r\nBetween\r\n<thread name="deleted-worker">\r\ntwo\r\n</thread>\r\nAfter',
      ),
    ).toEqual([
      { text: "Before\r" },
      { name: "alpha", text: "**one**\r" },
      { text: "Between\r" },
      { name: "deleted-worker", text: "two\r" },
      { text: "After" },
    ]);
  });

  it("ends a section at the next opening tag or EOF and drops stray closing tags", () => {
    expect(
      threadSections(
        '</thread>\n<thread name="a">\none\n<thread name="b">\ntwo',
      ),
    ).toEqual([
      { name: "a", text: "one" },
      { name: "b", text: "two" },
    ]);
    expect(render('<thread name="empty">\n</thread>')).toContain(
      'aria-label="empty"',
    );
  });

  it.each(["<", "<thr", '<thread name="alpha', "</thr"])(
    "holds back a streaming partial delimiter %s, but preserves it when final",
    (partial) => {
      expect(threadSections(`Intro\n${partial}`, true)).toEqual([
        { text: "Intro\n" },
      ]);
      expect(threadSections(`Intro\n${partial}`)).toEqual([
        { text: `Intro\n${partial}` },
      ]);
      expect(
        threadSections(`<thread name="a">\nBody\n${partial}`, true),
      ).toEqual([{ name: "a", text: "Body\n" }]);
    },
  );

  it("renders the body while streaming before its closing tag arrives", () => {
    const html = render('<thread name="a">\n**Working**\n</thr', {
      streaming: true,
    });
    expect(html).toContain("<strong>Working</strong>");
    expect(html).not.toContain("&lt;/thr");
  });

  it.each([
    'it said <thread name="a"> then stopped',
    '<thread name="a"> inline </thread>',
    '<thread name="">',
    '<thread name="   ">',
    '<thread name="a" extra="b">',
    '`<thread name="a">`',
    '> <thread name="a">',
  ])("leaves inline, quoted and malformed tags unchanged: %s", (text) => {
    expect(threadSections(text)).toEqual([{ text }]);
    expect(render(text)).not.toContain('class="thread-message"');
  });

  it.each(["```", "~~~~", "````"])(
    "keeps tags and partial tags literal in %s code fences",
    (fence) => {
      const code = `${fence}html\n<thread name="example">\ncode\n</thread>\n\n${fence}\n`;
      expect(threadSections(code)).toEqual([{ text: code }]);
      expect(threadSections(`${fence}\n<thr`, true)).toEqual([
        { text: `${fence}\n<thr` },
      ]);
      const html = render(
        `<thread name="real">\n${code}\nStill in the box.\n</thread>`,
      );
      expect(html.match(/class="thread-message"/g)).toHaveLength(1);
      expect(html).toContain("&lt;thread name=&quot;example&quot;&gt;");
      expect(html).toContain('aria-label="Copy code"');
      expect(html).toContain("Still in the box.");
    },
  );

  it("does not close a code fence with a shorter or different delimiter", () => {
    const code = '````html\n```\n~~~\n<thread name="example">\n</thread>\n````';
    expect(threadSections(code)).toEqual([{ text: code }]);
  });

  it("preserves indented code", () => {
    const code = '    <thread name="a">\n    code\n    </thread>';
    expect(threadSections(code)).toEqual([{ text: code }]);
    expect(render(code)).toContain("<pre><code>");
    expect(render(code)).not.toContain('class="thread-message"');
  });

  it("keeps Markdown, links, tables, and safe HTML handling in each section", () => {
    const html = render(
      'Intro **bold**\n\n<thread name="<img src=x onerror=alert(1)>">\n- [x] **Done** with `code`\n\n[docs](https://example.com)\n\n| A | B |\n| - | - |\n| 1 | 2 |\n\n<script>alert(1)</script>\n</thread>\nAfter *italic*',
    );
    expect(html).toContain("<strong>bold</strong>");
    expect(html).toContain("<strong>Done</strong>");
    expect(html).toContain('<input type="checkbox" disabled="" checked=""/>');
    expect(html).toContain("<code>code</code>");
    expect(html).toContain('title="https://example.com/"');
    expect(html).toContain("<table>");
    expect(html).toContain("<em>italic</em>");
    expect(html).not.toContain("<script>");
    expect(html).not.toContain("<img");
    expect(html).toContain("&lt;img src=x onerror=alert(1)&gt;");
  });

  it("leaves user, worker and tool rendering on the existing path", () => {
    const text = '<thread name="a">\n**body**\n</thread>';
    const ordinary = render(text, { threadTags: false });
    const tool = render(text, { mono: true });
    for (const html of [ordinary, tool]) {
      expect(html).not.toContain('class="thread-message"');
      expect(html).toContain("&lt;thread");
    }
    expect(tool).toContain("**body**");
  });
});
