import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { MessageBody } from "../shared/MessageBody";
import { THREAD_COLORS, threadInk } from "../shared/threadColor";
import type { HubEvent, Thread } from "../shared/types";
import { blocksOf } from "../shared/transcript";

vi.mock("../shared/host", () => ({
  useHostAPI: () => ({
    openExternal: async () => {},
    copyText: async () => {},
  }),
}));

const render = (
  text: string,
  options: {
    threadTags?: boolean;
    streaming?: boolean;
    mono?: boolean;
    threads?: Thread[];
    turnStartedAt?: string;
  } = {},
) =>
  renderToStaticMarkup(
    createElement(MessageBody, {
      text,
      clamp: false,
      threadTags: true,
      turnStartedAt: "2026-11-17T12:00:00Z",
      ...options,
    }),
  );

const boxes = (html: string) =>
  Array.from(
    html.matchAll(
      /<section class="thread-message" aria-label="([^"]*)"[^>]*>([\s\S]*?)<\/section>/g,
    ),
    (match) => ({ name: match[1], html: match[2] }),
  );

describe("orchestrator thread sections", () => {
  const worker = (overrides: Partial<Thread> = {}): Thread => ({
    id: "t1",
    name: "api",
    created_at: "2026-11-16T12:00:00Z",
    accent: THREAD_COLORS[1],
    runtime: "fake",
    box_name: "local",
    mode: "default",
    status: "idle",
    is_orchestrator: false,
    ...overrides,
  });
  const section = (name: string, threads: Thread[], streaming = false) =>
    render(
      `<thread name="${name}">\n**Working**\n${streaming ? "</thr" : "</thread>"}`,
      { threads, streaming },
    );

  it("uses each assigned palette color and both readable inks without coloring Markdown body text", () => {
    for (const accent of THREAD_COLORS) {
      const html = section("api", [worker({ accent })]);
      expect(html).toContain(`--thread-color:${accent}`);
      expect(html).toContain(
        `--thread-ink-light:${threadInk(accent, "light")}`,
      );
      expect(html).toContain(`--thread-ink-dark:${threadInk(accent, "dark")}`);
      expect(html).toContain(
        `--thread-ink-omarchy:var(--omarchy-ink-${accent.slice(1)}, var(--text))`,
      );
      expect(html).toContain('class="thread-message-name thread-ink"');
      expect(html).toMatch(
        /<div class="markdown">\s*<p><strong>Working<\/strong><\/p>/,
      );
    }
    const html = render(
      '<thread name="api">\nOne\n</thread>\n<thread name="docs">\nTwo\n</thread>',
      {
        threads: [
          worker(),
          worker({ id: "t2", name: "docs", accent: THREAD_COLORS[0] }),
        ],
      },
    );
    expect(html).toContain(`--thread-color:${THREAD_COLORS[0]}`);
    expect(html).toContain(`--thread-color:${THREAD_COLORS[1]}`);
  });

  it.each(["t1", "api", "backend", "old-api"])(
    "resolves exact identity %s including renamed threads",
    (name) => {
      expect(
        section(name, [
          worker({ aliases: ["backend"], former_names: ["old-api"] }),
        ]),
      ).toContain(`--thread-color:${THREAD_COLORS[1]}`);
    },
  );

  it("prefers ids, then current names, then aliases over former names, independent of row order", () => {
    const threads = [
      worker({
        id: "old",
        name: "renamed",
        former_names: ["api", "backend"],
        accent: THREAD_COLORS[2],
      }),
      worker({
        id: "other",
        name: "other",
        aliases: ["api", "backend"],
        accent: THREAD_COLORS[3],
      }),
      worker(),
      worker({ id: "api", name: "id-match", accent: THREAD_COLORS[4] }),
    ];
    for (const rows of [threads, [...threads].reverse()]) {
      expect(section("api", rows)).toContain(
        `--thread-color:${THREAD_COLORS[4]}`,
      );
      expect(
        section(
          "api",
          rows.filter((t) => t.id !== "api"),
        ),
      ).toContain(`--thread-color:${THREAD_COLORS[1]}`);
      expect(section("backend", rows)).toContain(
        `--thread-color:${THREAD_COLORS[3]}`,
      );
    }
  });

  it("keeps unknown, missing, invalid-color and ambiguous threads neutral without guessing prefixes", () => {
    for (const [name, threads] of [
      ["missing", [worker()]],
      ["api", []],
      ["ap", [worker()]],
      ["api", [worker({ accent: "invalid" })]],
      ["api", [worker(), worker({ id: "t2" })]],
      [
        "old",
        [
          worker({ former_names: ["old"] }),
          worker({ id: "t2", name: "docs", former_names: ["old"] }),
        ],
      ],
    ] as [string, Thread[]][]) {
      const html = section(name, threads);
      expect(html).not.toContain("--thread-color:");
      expect(boxes(html)).toHaveLength(1);
      expect(html).toContain("<strong>Working</strong>");
    }
    expect(
      section("api", [
        worker({ stopped: true }),
        worker({ id: "t2", accent: THREAD_COLORS[2] }),
      ]),
    ).toContain(`--thread-color:${THREAD_COLORS[2]}`);
  });

  it("colors streaming sections with known current and former names", () => {
    const html = section("api", [worker()], true);
    expect(html).toContain(`--thread-color:${THREAD_COLORS[1]}`);
    expect(html).toContain("<strong>Working</strong>");
    expect(html).not.toContain("&lt;/thr");
    expect(
      section(
        "api",
        [worker({ name: "renamed", former_names: ["api"] })],
        true,
      ),
    ).toContain(`--thread-color:${THREAD_COLORS[1]}`);
    expect(section("api", [], true)).not.toContain("--thread-color:");
  });

  it.each(["api", "backend"])(
    "does not color historical boxes after deletion and reuse of %s",
    (name) => {
      const text = `<thread name="${name}">\n**Original update**\n</thread>`;
      const original = worker({ aliases: ["backend"] });
      const replacement = worker({
        id: "t2",
        aliases: ["backend"],
        accent: THREAD_COLORS[2],
        created_at: "2026-11-17T12:00:01Z",
      });
      expect(render(text, { threads: [original] })).toContain(
        `--thread-color:${original.accent}`,
      );
      for (const threads of [[], [replacement]]) {
        const html = render(text, { threads });
        expect(html).not.toContain("--thread-color:");
        expect(html).toContain("<strong>Original update</strong>");
      }
      // Fresh renders model reloaded history, without a component-local identity cache.
      expect(render(text, { threads: [replacement] })).not.toContain(
        "--thread-color:",
      );
      expect(
        render(text, {
          threads: [replacement],
          turnStartedAt: "2026-11-17T12:00:02Z",
        }),
      ).toContain(`--thread-color:${replacement.accent}`);
    },
  );

  it.each([
    undefined,
    null,
    "invalid",
    "2026-11-17T12:00:00Z",
    "2026-11-17T12:00:00.000001Z",
    "2026-11-17T12:00:01Z",
  ])(
    "keeps a match neutral unless its creation time %s proves it predates the turn",
    (created_at) => {
      for (const name of ["t1", "api", "backend", "old-api"]) {
        expect(
          section(name, [
            worker({
              created_at,
              aliases: ["backend"],
              former_names: ["old-api"],
            }),
          ]),
        ).not.toContain("--thread-color:");
      }
    },
  );

  it.each([undefined, "invalid"])(
    "keeps identities neutral without a valid turn-start timestamp: %s",
    (turnStartedAt) => {
      expect(
        render('<thread name="api">\nBody\n</thread>', {
          threads: [worker()],
          turnStartedAt,
        }),
      ).not.toContain("--thread-color:");
    },
  );

  it("does not fall through a later-created exact name to a weaker alias match", () => {
    const threads = [
      worker({ id: "new", created_at: "2026-11-17T12:00:01Z" }),
      worker({ name: "other", aliases: ["api"] }),
    ];
    expect(section("api", threads)).not.toContain("--thread-color:");
  });

  it.each(["api", "backend"])(
    "keeps %s neutral live and in final-only history when replacement happens mid-reply",
    (name) => {
      const text = `<thread name="${name}">\n**Streaming update**\n</thread>`;
      const event = (
        seq: number,
        kind: string,
        data: HubEvent["data"],
        ts: string,
        turn = 1,
      ): HubEvent => ({
        id: `orch:${seq}`,
        thread: "orch",
        seq,
        turn,
        kind,
        data,
        ts,
      });
      const start = event(1, "turn.started", {}, "2026-11-17T12:00:00Z");
      const delta = event(
        2,
        "agent.text",
        { text, delta: true },
        "2026-11-17T12:00:01Z",
      );
      const final = event(
        3,
        "agent.text",
        { text, final: true },
        "2026-11-17T12:00:03Z",
      );
      const original = worker({ aliases: ["backend"] });
      const replacement = worker({
        id: "t2",
        aliases: ["backend"],
        accent: THREAD_COLORS[2],
        created_at: "2026-11-17T12:00:02Z",
      });
      const reply = (events: HubEvent[], thread: Thread) => {
        const block = blocksOf(events).find(
          (block) => block.kind === "assistant",
        )!;
        return {
          block,
          html: render(block.text, {
            threads: [thread],
            streaming: !!block.pending,
            turnStartedAt: block.turnStartedAt,
          }),
        };
      };
      expect(reply([start, delta], original).html).toContain(
        `--thread-color:${original.accent}`,
      );
      const live = reply([start, delta, final], replacement);
      // read_transcript_events retains turn.started and final text, never deltas.
      const history = reply([start, final], replacement);
      expect(live.block.ts).toBe(delta.ts);
      expect(history.block.ts).toBe(final.ts);
      for (const result of [
        reply([start, delta], replacement),
        live,
        history,
      ]) {
        expect(result.block.turnStartedAt).toBe(start.ts);
        expect(result.html).not.toContain("--thread-color:");
        expect(result.html).toContain("<strong>Streaming update</strong>");
      }
      const laterStart = event(
        4,
        "turn.started",
        {},
        "2026-11-17T12:00:04Z",
        2,
      );
      const laterFinal = event(
        5,
        "agent.text",
        { text, final: true },
        "2026-11-17T12:00:05Z",
        2,
      );
      expect(reply([laterStart, laterFinal], replacement).html).toContain(
        `--thread-color:${replacement.accent}`,
      );
    },
  );

  it("does not substitute a final timestamp or another turn/thread's start when the matching start is absent", () => {
    const final: HubEvent = {
      id: "orch:2",
      seq: 2,
      thread: "orch",
      turn: 2,
      kind: "agent.text",
      ts: "2026-11-17T12:00:03Z",
      data: { final: true, text: '<thread name="api">\nBody\n</thread>' },
    };
    const start = {
      ...final,
      id: "orch:1",
      seq: 1,
      kind: "turn.started",
      ts: "2026-11-17T12:00:00Z",
      data: {},
    };
    for (const events of [
      [final],
      [{ ...start, turn: 1 }, final],
      [{ ...start, thread: "other" }, final],
      [
        { ...start, turn: undefined },
        { ...final, turn: undefined },
      ],
    ]) {
      const block = blocksOf(events).find(
        (block) => block.kind === "assistant",
      )!;
      expect(block.turnStartedAt).toBeUndefined();
      expect(
        render(block.text, {
          threads: [worker()],
          turnStartedAt: block.turnStartedAt,
        }),
      ).not.toContain("--thread-color:");
    }
  });

  it("preserves a footnote definition atomically when its body contains thread tags", () => {
    const html = render(
      'Text[^n].\n\n[^n]: Before.\n    <thread name="a">\n    Inside.\n    </thread>\n    After.',
    );
    expect(boxes(html)).toHaveLength(0);
    expect(html.match(/id="user-content-fn-n"/g)).toHaveLength(1);
    expect(html).toContain("Before.");
    expect(html).toContain("Inside.");
    expect(html).toContain("After.");
    expect(html).toContain("&lt;thread name=&quot;a&quot;&gt;");
    expect(html).toContain("&lt;/thread&gt;");
  });

  it.each([
    [
      "a list within the footnote",
      'Text[^n].\n\n[^n]: Before.\n    - Item.\n      <thread name="a">\n      Inside.\n      </thread>\n      After.',
    ],
    [
      "a footnote within a list",
      '- Text[^n].\n\n  [^n]: Before.\n      <thread name="a">\n      Inside.\n      </thread>\n      After.',
    ],
    [
      "a footnote within a blockquote",
      '> Text[^n].\n>\n> [^n]: Before.\n>     <thread name="a">\n>     Inside.\n>     </thread>\n>     After.',
    ],
  ])(
    "keeps thread tags literal with %s, and still renders subsequent real boxes",
    (_context, definition) => {
      const html = render(
        `${definition}\n\n<thread name="real">\nWorker update.\n</thread>`,
      );
      expect(boxes(html).map((box) => box.name)).toEqual(["real"]);
      expect(boxes(html)[0].html).toContain("Worker update.");
      for (const content of ["Before.", "Inside.", "After."]) {
        expect(html.split(content)).toHaveLength(2);
        expect(boxes(html)[0].html).not.toContain(content);
      }
      expect(html.match(/id="user-content-fn-n"/g)).toHaveLength(1);
      expect(html).toContain("&lt;thread name=&quot;a&quot;&gt;");
      expect(html).toContain("&lt;/thread&gt;");
    },
  );

  it("keeps a footnote defined inside a real thread whole and renders it only in the footer", () => {
    const html = render(
      '<thread name="outer">\nText[^n].\n\n[^n]: Before.\n    <thread name="a">\n    Inside.\n    </thread>\n    After.\n</thread>\nOutside prose',
    );
    const sections = boxes(html);
    expect(sections.map((section) => section.name)).toEqual(["outer"]);
    expect(sections[0].html).toContain("Text");
    expect(sections[0].html).not.toContain("Before.");
    expect(sections[0].html).not.toContain("Inside.");
    expect(sections[0].html).not.toContain("After.");
    expect(html).toMatch(
      /class="footnotes"[\s\S]*Before\.[\s\S]*Inside\.[\s\S]*After\./,
    );
    expect(html.match(/id="user-content-fn-n"/g)).toHaveLength(1);
    expect(html).toMatch(
      /<\/section>\s*<div class="markdown">\s*<p>Outside prose/,
    );
  });

  it("shares a single complete footnote across references inside and outside a box", () => {
    const html = render(
      'Text[^n].\n\n<thread name="outer">\nWorker update[^n].\n</thread>\n\n[^n]: Before.\n    <thread name="a">\n    Inside.\n    </thread>\n    After.',
    );
    expect(boxes(html).map((box) => box.name)).toEqual(["outer"]);
    expect(html.match(/title="#user-content-fn-n"/g)).toHaveLength(2);
    expect(html.match(/id="user-content-fn-n"/g)).toHaveLength(1);
    expect(html).toMatch(
      /class="footnotes"[\s\S]*Before\.[\s\S]*Inside\.[\s\S]*After\./,
    );
  });

  it.each(['<thread name="a', "</thr"])(
    "preserves a partial tag %s literally inside a streaming footnote",
    (partial) => {
      const html = render(`Text[^n].\n\n[^n]: Before.\n    ${partial}`, {
        streaming: true,
      });
      expect(boxes(html)).toHaveLength(0);
      expect(html.match(/id="user-content-fn-n"/g)).toHaveLength(1);
      expect(html).toContain("Before.");
      expect(html).toContain(
        partial.replace("<", "&lt;").replaceAll('"', "&quot;"),
      );
    },
  );

  it("preserves link definitions nested in an atomic footnote and resolves their references document-wide", () => {
    const html = render(
      'Text[^n] and [outside][d].\n\n[^n]: Before.\n    <thread name="a">\n    Inside [docs][d].\n    </thread>\n    After.\n\n    [d]: https://example.com/first "Footnote docs"\n\n[d]: https://example.com/second',
    );
    expect(boxes(html)).toHaveLength(0);
    expect(html.match(/id="user-content-fn-n"/g)).toHaveLength(1);
    expect(html.match(/href="https:\/\/example.com\/first"/g)).toHaveLength(2);
    expect(html).not.toContain('href="https://example.com/second"');
    expect(html).not.toContain("[docs][d]");
    expect(html).not.toContain("[outside][d]");
    expect(html).toContain("Before.");
    expect(html).toContain("After.");
  });

  it("ends a section at an indented closing delimiter after a list item", () => {
    const html = render(
      '<thread name="a">\n- Done\n  </thread>\nOutside prose',
    );
    expect(html).toContain("<li>Done</li>");
    expect(html).not.toContain("&lt;/thread");
    expect(html).toMatch(
      /<\/section>\s*<div class="markdown">\s*<p>Outside prose/,
    );
  });

  it("starts and ends a section within a list, leaving surrounding list content outside", () => {
    const html = render(
      '- Before\n  <thread name="a">\n  Inside\n  </thread>\n  After',
    );
    expect(html.match(/class="thread-message"/g)).toHaveLength(1);
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
    expect(html).toMatch(
      /Before[\s\S]*<section class="thread-message" aria-label="a">[\s\S]*Inside[\s\S]*<\/section>[\s\S]*After/,
    );
    expect(html.match(/<ul>/g)).toHaveLength(3);
  });

  it("switches sections at an indented opening delimiter within a list", () => {
    const html = render(
      '<thread name="a">\n- Done\n  <thread name="b">\n  Working\n  </thread>\nOutside prose',
    );
    expect(html.match(/class="thread-message"/g)).toHaveLength(2);
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
    expect(html).toMatch(
      /aria-label="a">[\s\S]*Done[\s\S]*<\/section>\s*<section class="thread-message" aria-label="b">[\s\S]*Working[\s\S]*<\/section>\s*<div class="markdown">\s*<p>Outside prose/,
    );
  });

  it.each([
    ["ordered list", "7. Done", "   "],
    ["nested list", "- Parent\n  - Done", "    "],
    ["loose list", "- First\n\n- Done", "  "],
    ["task list", "- [x] Done", "  "],
    ["quote with a list", "> - Done", "  "],
  ])(
    "closes a thread after a %s without consuming outside prose",
    (_kind, content, indent) => {
      const html = render(
        `<thread name="a">\n${content}\n${indent}</thread>\nOutside prose`,
      );
      const sections = boxes(html);
      expect(sections).toHaveLength(1);
      expect(sections[0].html).toContain("Done");
      expect(sections[0].html).not.toContain("Outside prose");
      expect(html).not.toContain("&lt;/thread");
      expect(html).toMatch(
        /<\/section>\s*<div class="markdown">\s*<p>Outside prose/,
      );
    },
  );

  it.each([
    ["ordered list", "7. Before", "   "],
    ["nested list", "- Parent\n  - Before", "    "],
    ["loose list", "- First\n\n- Before", "  "],
    ["blockquote", "> Before", "  "],
  ])("opens and closes a thread after a %s", (_kind, content, indent) => {
    const html = render(
      `${content}\n${indent}<thread name="a">\n${indent}Inside\n${indent}</thread>\nOutside prose`,
    );
    const sections = boxes(html);
    expect(sections).toHaveLength(1);
    expect(sections[0].name).toBe("a");
    expect(sections[0].html).toContain("Inside");
    expect(sections[0].html).not.toContain("Before");
    expect(sections[0].html).not.toContain("Outside prose");
    expect(html).toContain("Before");
    expect(html).toContain("Outside prose");
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
  });

  it("preserves ordered list numbering when a boundary splits the list", () => {
    const html = render(
      '<thread name="a">\n7. First\n8. Done\n   </thread>\n9. Outside',
    );
    expect(boxes(html)[0].html).toContain('<ol start="7">');
    expect(boxes(html)[0].html.match(/<li>/g)).toHaveLength(2);
    expect(html).toMatch(
      /<\/section>\s*<div class="markdown">\s*<ol start="9">[\s\S]*Outside/,
    );
    expect(html).not.toContain("&lt;/thread");
  });

  it("preserves list numbering when opening and closing inside the same ordered item", () => {
    const html = render(
      '7. Before\n   <thread name="a">\n   Inside\n   </thread>\n8. After',
    );
    expect(boxes(html)[0].html).toContain('<ol start="7">');
    expect(html).toMatch(
      /<\/section>\s*<div class="markdown">\s*<ol start="8">[\s\S]*After/,
    );
  });

  it("does not duplicate task checkboxes across fragments of the same item", () => {
    const html = render(
      '- [x] Done\n  <thread name="a">\n  More\n  </thread>\n  Continuing\n- [ ] Next',
    );
    expect(html.match(/type="checkbox"/g)).toHaveLength(2);
    expect(html.match(/ checked=""/g)).toHaveLength(1);
    expect(boxes(html)[0].html).not.toContain("<input");
    expect(boxes(html)[0].html).toContain("More");
    expect(html).toContain("Continuing");
    expect(html).toContain("Next");
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
  });

  it("switches between thread sections at consecutive nested delimiters", () => {
    const html = render(
      '<thread name="a">\n- Done\n  </thread>\n  <thread name="b">\n  Working\n  </thread>\nOutside prose',
    );
    const sections = boxes(html);
    expect(sections.map((section) => section.name)).toEqual(["a", "b"]);
    expect(sections[0].html).toContain("Done");
    expect(sections[0].html).not.toContain("Working");
    expect(sections[1].html).toContain("Working");
    expect(sections[1].html).not.toContain("Outside prose");
    expect(html).not.toContain("&lt;thread");
    expect(html).not.toContain("&lt;/thread");
  });

  it.each(["</thr", '<thread name="b'])(
    "hides streaming nested partial delimiter %s without losing the list",
    (partial) => {
      const text = `<thread name="a">\n- Done\n  ${partial}`;
      const html = render(text, { streaming: true });
      expect(boxes(html)).toHaveLength(1);
      expect(boxes(html)[0].html).toContain("<li>Done</li>");
      expect(html).not.toContain("&lt;/thr");
      expect(html).not.toContain("&lt;thread");
      const final = render(
        '<thread name="a">\n- Done\n  </thread>\nOutside prose',
      );
      expect(boxes(final)[0].html).not.toContain("Outside prose");
    },
  );

  it("keeps nested fence examples literal while an indented delimiter closes the real box", () => {
    const html = render(
      '<thread name="a">\n- ```xml\n  <thread name="example">\n  literal\n  </thread>\n  ```\n  </thread>\nOutside prose',
    );
    const sections = boxes(html);
    expect(sections).toHaveLength(1);
    expect(sections[0].html).toContain(
      "&lt;thread name=&quot;example&quot;&gt;",
    );
    expect(sections[0].html).toContain("&lt;/thread&gt;");
    expect(sections[0].html).not.toContain("Outside prose");
    expect(html).toMatch(
      /<\/section>\s*<div class="markdown">\s*<p>Outside prose/,
    );
  });

  it("shares reference definitions across lifted section boundaries", () => {
    const html = render(
      'Before [docs][d].\n\n- Done\n  <thread name="a">\n  Inside [docs][d].\n  </thread>\nOutside [docs][d].\n\n[d]: https://example.com',
    );
    expect(html.match(/href="https:\/\/example.com\/"/g)).toHaveLength(3);
    expect(boxes(html)[0].html).toContain("Inside");
    expect(boxes(html)[0].html).not.toContain("Outside");
    expect(boxes(html)[0].html).not.toContain("Before");
    expect(html).not.toContain("[docs][d]");
  });

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
      '<div class="thread-message-name thread-ink">install-topbar-fix</div>',
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
