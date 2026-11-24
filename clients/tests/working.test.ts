import { afterEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { installFakeDOM, type FakeElement } from "./fake-dom";
import type { HubEvent } from "../shared/types";
import {
  AWAIT_LIMIT_MS,
  animateChange,
  animateSetting,
  conversationWorks,
  cursorHost,
  cursorPlace,
  lastTurnEnd,
  stillAwaited,
  threadWorks,
  waitsOnUser,
} from "../shared/working";

vi.mock("../shared/host", () => ({
  useHostAPI: () => ({
    openExternal: async () => {},
    copyText: async () => {},
  }),
}));

const thread = (status: string) => ({ id: "t1", status });
const event = (kind: string, id: string, data: object = {}): HubEvent => ({
  id,
  seq: Number(id.replace(/\D/g, "")) || 0,
  kind,
  thread: "t1",
  ts: "2026-11-23T10:00:00Z",
  data,
});

describe("what counts as working", () => {
  it("is a running turn or background tasks, as the TUI's icon", () => {
    expect(threadWorks(thread("running"))).toBe(true);
    expect(threadWorks(thread("background"))).toBe(true);
    for (const status of [
      "waiting",
      "idle",
      "done",
      "failed",
      "parked",
      "attached",
    ])
      expect(threadWorks(thread(status)), status).toBe(false);
    expect(threadWorks(undefined)).toBe(false);
  });

  it("never while the thread waits on a permission or a question", () => {
    expect(waitsOnUser(thread("waiting"), [])).toBe(true);
    // The request is in the view before the status catches up.
    for (const reason of ["permission", "question"])
      expect(
        conversationWorks({
          thread: thread("running"),
          attention: [{ thread: "t1", reason }],
          sending: true,
          awaited: true,
        }),
        reason,
      ).toBe(false);
    // Another thread's request, or a stall, doesn't stop it.
    expect(
      conversationWorks({
        thread: thread("running"),
        attention: [
          { thread: "t2", reason: "permission" },
          { thread: "t1", reason: "stalled" },
        ],
        sending: false,
        awaited: false,
      }),
    ).toBe(true);
  });

  it("starts as the message is sent, before the hub says the turn runs", () => {
    const idle = thread("idle");
    const works = (sending: boolean, awaited: boolean) =>
      conversationWorks({ thread: idle, attention: [], sending, awaited });
    expect(works(false, false)).toBe(false);
    expect(works(true, false)).toBe(true);
    expect(works(false, true)).toBe(true);
    expect(
      conversationWorks({
        thread: undefined,
        attention: [],
        sending: true,
        awaited: true,
      }),
    ).toBe(false);
  });
});

describe("the gap between the send and the turn", () => {
  const before = [event("turn.started", "e1"), event("turn.ended", "e2")];
  const awaited = { thread: "t1", endedBefore: "e2", at: 1000 };

  it("is bridged until the hub says the thread works or waits", () => {
    expect(lastTurnEnd(before)).toBe("e2");
    expect(stillAwaited(awaited, thread("idle"), before, 2000)).toBe(true);
    for (const status of ["running", "background", "waiting"])
      expect(stillAwaited(awaited, thread(status), before, 2000), status).toBe(
        false,
      );
  });

  it("ends when a turn ends after the send, even one the status never showed", () => {
    const after = [
      ...before,
      event("turn.started", "e3"),
      event("turn.ended", "e4"),
    ];
    expect(stillAwaited(awaited, thread("idle"), after, 2000)).toBe(false);
  });

  it("ends on its own, and belongs to the thread it was sent to", () => {
    expect(
      stillAwaited(awaited, thread("idle"), before, 1000 + AWAIT_LIMIT_MS),
    ).toBe(false);
    expect(
      stillAwaited(awaited, { id: "t2", status: "idle" }, before, 2000),
    ).toBe(false);
    expect(stillAwaited(undefined, thread("idle"), before, 2000)).toBe(false);
  });
});

describe("where the cursor goes", () => {
  it("at the end of a streaming reply, else on a line of its own", () => {
    expect(cursorPlace({ kind: "assistant", pending: true })).toBe("inline");
    expect(cursorPlace({ kind: "assistant", pending: false })).toBe("line");
    expect(cursorPlace({ kind: "user" })).toBe("line");
    expect(cursorPlace({ kind: "tool", pending: true })).toBe("line");
    expect(cursorPlace({ kind: "thinking" })).toBe("line");
  });

  it("follows the last visible text, however the Markdown nests it", () => {
    const document = installFakeDOM();
    const root = document.createElement("div");
    const p = document.createElement("p");
    p.appendChild(document.createTextNode("First"));
    const list = document.createElement("ul");
    const item = document.createElement("li");
    const strong = document.createElement("strong");
    strong.appendChild(document.createTextNode("last"));
    item.appendChild(strong);
    item.appendChild(document.createTextNode("  \n"));
    list.appendChild(item);
    root.appendChild(p);
    root.appendChild(list);
    root.appendChild(document.createTextNode("\n"));
    expect(cursorHost(root as unknown as Element)).toBe(strong);
    const empty = document.createElement("div");
    expect(cursorHost(empty as unknown as Element)).toBe(empty);
  });
});

describe("the animate setting", () => {
  it("follows [tui] animate, as the file says it now", () => {
    expect(animateSetting(undefined)).toBe(true);
    expect(animateSetting({ tui: { animate: false } })).toBe(false);
    expect(
      animateSetting({
        tui: { animate: false },
        preferences: { "tui.animate": true },
      }),
    ).toBe(true);
    // Unset in the file: what the hub loaded.
    expect(
      animateSetting({
        tui: { animate: false },
        preferences: { "tui.animate": null },
      }),
    ).toBe(false);
  });

  it("is read again on each new config.changed of tui.animate", () => {
    expect(animateChange([])).toBeUndefined();
    expect(
      animateChange([
        event("config.changed", "e1", { key: "tui.animate", value: false }),
        event("config.changed", "e2", { key: "tui.mouse", value: true }),
      ]),
    ).toBe("e1");
    expect(
      animateChange([
        event("config.changed", "e1", { key: "tui.animate", value: false }),
        event("config.changed", "e3", { key: "tui.animate", value: true }),
      ]),
    ).toBe("e3");
  });
});

describe("the cursor in a reply", () => {
  let unmount: (() => Promise<void>) | undefined;
  afterEach(async () => {
    await unmount?.();
    unmount = undefined;
  });

  async function mount() {
    const document = installFakeDOM();
    (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
    const { act, createElement } = await import("react");
    const { createRoot } = await import("react-dom/client");
    const { MessageBody, CURSOR_ATTRIBUTE } =
      await import("../shared/MessageBody");
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container as unknown as Element);
    const render = (props: { text: string; working: boolean }) =>
      act(async () => root.render(createElement(MessageBody, props)));
    unmount = () => act(async () => root.unmount());
    const hosts = () =>
      container.querySelectorAll(`[${CURSOR_ATTRIBUTE}]`) as FakeElement[];
    return { container, render, hosts };
  }

  it("marks the element holding the reply's last text while the turn works", async () => {
    const { render, hosts } = await mount();
    await render({ text: "Looking at it.\n\n- one\n- **two**", working: true });
    expect(hosts().map((host) => host.tagName.toLowerCase())).toEqual([
      "strong",
    ]);
    expect(hosts()[0].textContent).toBe("two");
    await render({
      text: "Looking at it.\n\n- one\n- **two**",
      working: false,
    });
    expect(hosts()).toHaveLength(0);
  });

  it("waits beside Show full message while a long reply is collapsed", async () => {
    const { container, render, hosts } = await mount();
    const long = Array.from({ length: 40 }, (_, n) => `line ${n}`).join("\n\n");
    await render({ text: long, working: true });
    expect(hosts()).toHaveLength(0);
    expect(container.querySelectorAll(".message-working")).toHaveLength(1);
    await render({ text: long, working: false });
    expect(container.querySelectorAll(".message-working")).toHaveLength(0);
  });
});

describe("the cursor's look", () => {
  const css = readFileSync(
    new URL("../shared/style.css", import.meta.url),
    "utf8",
  );
  const rust = readFileSync(
    new URL("../../crates/twr-tui/src/icon.rs", import.meta.url),
    "utf8",
  );

  it("breathes through the TUI's frames, 125 ms each", () => {
    const frames = [
      ...rust
        .match(/pub const FRAMES: \[char; (\d+)\] = \[([^\]]+)\]/)![2]
        .matchAll(/'(.)'/g),
    ].map((m) => m[1]);
    expect(rust).toContain("Duration::from_millis(125)");
    const block = css.match(/@keyframes working-cursor \{([\s\S]*?)\n\}/)![1];
    // Each frame's height, by the time it starts.
    const heights = new Map<number, number>();
    for (const rule of block.matchAll(/([\d.%,\s]+)\{\s*scale: 1 ([\d.]+);/g))
      for (const at of rule[1].split(","))
        heights.set(parseFloat(at), parseFloat(rule[2]));
    const shown = frames.map((_, n) =>
      heights.get(+((n * 100) / 12).toFixed(3)),
    );
    // `_`, then ▁ (1/8) up to ▆ (6/8) and back down.
    const eighths: Record<string, number> = {
      "▁": 1,
      "▂": 2,
      "▃": 3,
      "▄": 4,
      "▅": 5,
      "▆": 6,
    };
    expect(frames[0]).toBe("_");
    frames
      .slice(1)
      .forEach((frame, n) =>
        expect(shown[n + 1], frame).toBe(eighths[frame] / 8),
      );
    expect(shown[0]).toBeLessThan(1 / 8);
    expect(css).toMatch(
      new RegExp(
        `animation: working-cursor ${(frames.length * 125) / 1000}s step-end infinite`,
      ),
    );
  });

  it("stays a still _ with reduced motion or animate = false", () => {
    expect(css).toMatch(
      /@media \(prefers-reduced-motion: reduce\) \{\s*\.working-cursor,\s*\[data-working-cursor\]::after \{\s*animation: none;/,
    );
    expect(css).toMatch(
      /\.conversation-scroll\[data-still\] \.working-cursor,\s*\.conversation-scroll\[data-still\] \[data-working-cursor\]::after \{\s*animation: none;/,
    );
  });

  it("takes no room, so it never moves the view", () => {
    const rule = css.match(
      /\.working-cursor,\s*\[data-working-cursor\]::after \{([^}]*)\}/,
    )![1];
    expect(rule).toContain("width: 0.6em;");
    expect(rule).toContain("margin-right: -0.6em;");
    expect(css).toMatch(/\.working-line \{\s*position: relative;\s*height: 0;/);
    expect(css).toMatch(
      /\.working-line \.working-cursor \{\s*position: absolute;/,
    );
  });
});
