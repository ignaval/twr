import { describe, expect, it } from "vitest";
import {
  attentionAnnouncement,
  createAnnouncer,
  rovingIndex,
  trapTarget,
  turnAnnouncement,
} from "../shared/a11y";

class FakeNode {
  attributes = new Map<string, string>();
  textContent = "";
  className = "";
  isConnected = true;
  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }
}
function fakeDocument() {
  const children: FakeNode[] = [];
  return {
    children,
    doc: {
      createElement: () => new FakeNode(),
      body: { appendChild: (node: FakeNode) => children.push(node) },
    } as unknown as Pick<Document, "createElement" | "body">,
  };
}
function fakeClock() {
  let now = 0;
  let id = 0;
  const timers = new Map<number, { at: number; run: () => void }>();
  return {
    now: () => now,
    schedule: (run: () => void, ms: number) => {
      timers.set(++id, { at: now + ms, run });
      return id;
    },
    cancel: (handle: unknown) => timers.delete(handle as number),
    advance(ms: number) {
      now += ms;
      for (const [key, timer] of [...timers].sort((a, b) => a[1].at - b[1].at))
        if (timer.at <= now) {
          timers.delete(key);
          timer.run();
        }
    },
  };
}

describe("announcer", () => {
  it("creates one polite and one assertive region marked to survive modals", () => {
    const { doc, children } = fakeDocument();
    const announcer = createAnnouncer(doc, fakeClock());
    announcer.prepare();
    announcer.prepare();
    expect(children).toHaveLength(2);
    expect(children.map((node) => node.attributes.get("aria-live"))).toEqual([
      "polite",
      "assertive",
    ]);
    for (const node of children) {
      expect(node.attributes.get("aria-atomic")).toBe("true");
      expect(node.attributes.has("data-modal-keep")).toBe(true);
    }
  });

  it("coalesces bursts, drops quick repeats and clears stale text", () => {
    const { doc, children } = fakeDocument();
    const clock = fakeClock();
    const announcer = createAnnouncer(doc, {
      ...clock,
      delay: 100,
      repeatWindow: 1000,
      clearAfter: 5000,
    });
    announcer.announce("Connected to default");
    announcer.announce("  Connected   to default ");
    announcer.announce("Message sent");
    const polite = children[0];
    expect(polite.textContent).toBe("");
    clock.advance(100);
    expect(polite.textContent).toBe("Connected to default. Message sent");
    clock.advance(200);
    announcer.announce("Message sent");
    clock.advance(100);
    // The duplicate inside the repeat window is ignored, leaving the text alone.
    expect(polite.textContent).toBe("Connected to default. Message sent");
    clock.advance(5000);
    expect(polite.textContent).toBe("");
    announcer.announce("Message sent");
    clock.advance(100);
    expect(polite.textContent).toBe("Message sent");
    clock.advance(5000);
    expect(polite.textContent).toBe("");
  });

  it("keeps assertive messages separate and ignores empty text", () => {
    const { doc, children } = fakeDocument();
    const clock = fakeClock();
    const announcer = createAnnouncer(doc, { ...clock, delay: 50 });
    announcer.announce("   ");
    expect(children).toHaveLength(0);
    announcer.announce("Upload failed", "assertive");
    clock.advance(50);
    expect(children).toHaveLength(1);
    expect(children[0].attributes.get("aria-live")).toBe("assertive");
    expect(children[0].textContent).toBe("Upload failed");
  });
});

describe("focus trap", () => {
  it("wraps Tab at the edges and pulls stray focus back in", () => {
    expect(trapTarget(3, 2, false)).toBe(0);
    expect(trapTarget(3, 0, true)).toBe(2);
    expect(trapTarget(3, 1, false)).toBeUndefined();
    expect(trapTarget(3, 1, true)).toBeUndefined();
    expect(trapTarget(3, -1, false)).toBe(0);
    expect(trapTarget(3, -1, true)).toBe(2);
    expect(trapTarget(0, -1, false)).toBeUndefined();
  });
});

describe("roving keys", () => {
  it("moves through vertical menus with wrap, Home and End", () => {
    expect(rovingIndex("ArrowDown", 0, 3)).toBe(1);
    expect(rovingIndex("ArrowDown", 2, 3)).toBe(0);
    expect(rovingIndex("ArrowUp", 0, 3)).toBe(2);
    expect(rovingIndex("ArrowDown", -1, 3)).toBe(0);
    expect(rovingIndex("ArrowUp", -1, 3)).toBe(2);
    expect(rovingIndex("Home", 2, 3)).toBe(0);
    expect(rovingIndex("End", 0, 3)).toBe(2);
    expect(rovingIndex("ArrowRight", 0, 3)).toBeUndefined();
    expect(rovingIndex("Enter", 0, 3)).toBeUndefined();
    expect(rovingIndex("ArrowDown", 0, 0)).toBeUndefined();
  });

  it("uses left and right for horizontal tab lists", () => {
    expect(rovingIndex("ArrowRight", 0, 2, "horizontal")).toBe(1);
    expect(rovingIndex("ArrowLeft", 0, 2, "horizontal")).toBe(1);
    expect(rovingIndex("ArrowDown", 0, 2, "horizontal")).toBeUndefined();
    expect(rovingIndex("ArrowDown", 0, 2, "both")).toBe(1);
  });
});

describe("announcement text", () => {
  it("describes who needs the person and why", () => {
    expect(
      attentionAnnouncement({
        reason: "permission",
        summary: "Run   npm test",
        threadName: "api",
      }),
    ).toBe("Needs you: api asks for permission: Run npm test");
    expect(attentionAnnouncement({ reason: "question" })).toBe(
      "Needs you: twr has a question",
    );
    expect(attentionAnnouncement({ reason: "review", threadName: "web" })).toBe(
      "Needs you: web finished work to review",
    );
    expect(
      attentionAnnouncement({ reason: "x", summary: "a".repeat(400) }).length,
    ).toBeLessThan(200);
  });

  it("reports how a turn ended", () => {
    expect(turnAnnouncement("Orchestrator", "ok")).toBe(
      "Orchestrator finished responding",
    );
    expect(turnAnnouncement("api", "error")).toBe("api stopped with an error");
    expect(turnAnnouncement("api", "interrupted")).toBe("api was interrupted");
  });
});
