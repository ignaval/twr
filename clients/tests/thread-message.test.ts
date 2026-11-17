import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { MessageBody } from "../shared/MessageBody";

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
  it.each([
    '- ```xml\n  <thread name="example">\n  literal content\n  </thread>\n  ```',
    '1. ~~~xml\n   <thread name="example">\n   literal content\n   </thread>\n   ~~~',
    '- item\n  - ```xml\n    <thread name="example">\n    literal content\n    </thread>\n    ```',
    '> - ```xml\n>   <thread name="example">\n>   literal content\n>   </thread>\n>   ```',
    '- ```xml\r\n  <thread name="example">\r\n  literal content\r\n  </thread>\r\n  ```',
  ])(
    "preserves container-nested code and renders later real boxes: %s",
    (code) => {
      for (const boxed of [false, true]) {
        const prefix = boxed
          ? `<thread name="code">\n${code}\n</thread>`
          : code;
        const html = render(
          `${prefix}\n\n<thread name="real">\nActual update.\n</thread>`,
        );
        expect(html.match(/class="thread-message"/g)).toHaveLength(
          boxed ? 2 : 1,
        );
        expect(html).toContain('aria-label="real"');
        expect(html).toContain('class="language-xml"');
        expect(html).toContain("&lt;thread name=&quot;example&quot;&gt;");
        expect(html).toMatch(/literal content\r?\n&lt;\/thread&gt;/);
        expect(html).toContain("Actual update.");
      }
    },
  );

  it("keeps partial tags literal in a streaming list fence", () => {
    const html = render('- ```xml\n  <thread name="example">\n  </thr', {
      streaming: true,
    });
    expect(html).not.toContain('class="thread-message"');
    expect(html).toContain("&lt;thread name=&quot;example&quot;&gt;");
    expect(html).toContain("&lt;/thr");
  });

  it("resolves references throughout the whole reply across thread boundaries", () => {
    const html = render(
      'Before [docs][d].\n\n<thread name="first">\nInside [docs][d].\n</thread>\n\n<thread name="second">\nAlso [docs][d].\n</thread>\n\nAfter [docs][d].\n\n[d]: https://example.com',
    );
    expect(html.match(/href="https:\/\/example.com\/"/g)).toHaveLength(4);
    expect(html).not.toContain("[docs][d]");
    expect(html.match(/class="thread-message"/g)).toHaveLength(2);
  });

  it.each([
    '[d]: https://example.com\n\n<thread name="a">\n[docs][d]\n</thread>',
    '<thread name="a">\n[docs][d]\n</thread>\n[d]: https://example.com',
    'Before [docs][d].\n\n<thread name="a">\n[d]: https://example.com\n</thread>',
    '<thread name="a">\n[d]: https://example.com\n</thread>\nAfter [docs][d].',
    '<thread name="a">\n[docs][d]\n</thread>\n<thread name="b">\n[d]: https://example.com\n</thread>',
  ])(
    "resolves definitions before, after or inside another section: %s",
    (text) => {
      const html = render(text);
      expect(html).toContain('href="https://example.com/"');
      expect(html).not.toContain("[docs][d]");
    },
  );

  it("uses document-wide identifier normalization and the first real definition", () => {
    const html = render(
      '[Doc Label]: https://example.com/first "First"\n\n<thread name="a">\n[docs][DOC  LABEL] and [Doc Label][] and [Doc Label].\n\n[doc label]: https://example.com/second\n</thread>\n\n[docs][doc label]',
    );
    expect(html.match(/href="https:\/\/example.com\/first"/g)).toHaveLength(4);
    expect(html).not.toContain('href="https://example.com/second"');
    expect(html).not.toContain("[docs]");
  });

  it("does not collect reference definitions from nested code examples", () => {
    const html = render(
      '- ```md\n  [d]: https://example.com/code\n  ```\n\n<thread name="a">\n[docs][d]\n</thread>\n\n[d]: https://example.com/real',
    );
    expect(html).toContain("[d]: https://example.com/code");
    expect(html).toContain('href="https://example.com/real"');
    expect(html).not.toContain('href="https://example.com/code"');
  });

  it("resolves a streamed reference once its definition arrives", () => {
    const prefix = '<thread name="a">\n[docs][d]\n';
    const initial = render(prefix + "</thr", { streaming: true });
    expect(initial).toContain("[docs][d]");
    expect(initial).not.toContain("&lt;/thr");
    const final = render(prefix + "</thread>\n[d]: https://example.com");
    expect(final).toContain('href="https://example.com/"');
    expect(final).not.toContain("[docs][d]");
  });

  it("shares GFM footnote definitions across prose and thread sections", () => {
    const html = render(
      'Before[^note].\n\n<thread name="a">\nInside[^note].\n</thread>\n\n[^note]: One shared footnote.',
    );
    expect(html.match(/title="#user-content-fn-note"/g)).toHaveLength(2);
    expect(html.match(/id="user-content-fn-note"/g)).toHaveLength(1);
    expect(html).toContain("One shared footnote.");
    expect(html).not.toContain("Inside[^note]");
  });

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
    expect(html).toMatch(
      /<\/section>\s*<div class="markdown">\s*<p>Once verified/,
    );
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
  });

  it.each(["\n", "\r\n", "\r"])(
    "keeps prose and multiple sections in order with %j line endings",
    (newline) => {
      const html = render(
        'Before\r\n  <thread name=" alpha ">  \r\n**one**\r\n </thread> \r\nBetween\r\n<thread name="deleted-worker">\r\ntwo\r\n</thread>\r\nAfter'.replaceAll(
          "\r\n",
          newline,
        ),
      );
      expect(html).toMatch(
        /Before[\s\S]*aria-label="alpha"[\s\S]*<strong>one<\/strong>[\s\S]*Between[\s\S]*aria-label="deleted-worker"[\s\S]*two[\s\S]*After/,
      );
      expect(html.match(/class="thread-message"/g)).toHaveLength(2);
      expect(html).not.toContain("&lt;thread");
    },
  );

  it("ends a section at the next opening tag or EOF and drops stray closing tags", () => {
    const html = render(
      '</thread>\n<thread name="a">\none\n<thread name="b">\ntwo',
    );
    expect(html).toMatch(
      /aria-label="a"[\s\S]*one[\s\S]*<\/section>\s*<section class="thread-message" aria-label="b"[\s\S]*two/,
    );
    expect(html).not.toContain("&lt;/thread");
    expect(render('<thread name="empty">\n</thread>')).toContain(
      'aria-label="empty"',
    );
  });

  it.each(["<", "<thr", '<thread name="alpha', "</thr"])(
    "holds back a streaming partial delimiter %s, but preserves it when final",
    (partial) => {
      const escaped = partial.replace("<", "&lt;").replaceAll('"', "&quot;");
      expect(render(`Intro\n${partial}`, { streaming: true })).not.toContain(
        escaped,
      );
      expect(render(`Intro\n${partial}`)).toContain(escaped);
      const html = render(`<thread name="a">\nBody\n${partial}`, {
        streaming: true,
      });
      expect(html).toContain('aria-label="a"');
      expect(html).toContain("<p>Body</p>");
      expect(html).not.toContain(escaped);
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
    expect(render(text)).not.toContain('class="thread-message"');
    expect(render(text)).toContain("&lt;thread");
  });

  it.each(["```", "~~~~", "````"])(
    "keeps tags and partial tags literal in %s code fences",
    (fence) => {
      const code = `${fence}html\n<thread name="example">\ncode\n</thread>\n\n${fence}\n`;
      expect(render(code)).not.toContain('class="thread-message"');
      expect(render(`${fence}\n<thr`, { streaming: true })).toContain(
        "&lt;thr",
      );
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
    const html = render(code);
    expect(html).not.toContain('class="thread-message"');
    expect(html).toContain("&lt;thread name=&quot;example&quot;&gt;");
  });

  it("preserves indented code", () => {
    const code = '    <thread name="a">\n    code\n    </thread>';
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
