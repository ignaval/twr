import { describe, expect, it } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { StatusStrip } from "../shared/StatusStrip";
import { ActionsMenu } from "../shared/ActionsMenu";
import { PermissionModePicker, ThreadActions } from "../shared/ThreadActions";
import { eventRole, eventTime } from "../shared/InboxSearch";
import type { Thread } from "../shared/types";

const thread = (extra: Partial<Thread> = {}) =>
  ({
    id: "w1",
    name: "api",
    status: "running",
    mode: "auto",
    runtime: "claude-code",
    ...extra,
  }) as Thread;
const all = () => true;
const render = (type: any, props: any) =>
  renderToStaticMarkup(createElement(type, props));

describe("the status strip", () => {
  it("shows the first notice and counts the others", () => {
    const html = render(StatusStrip, {
      items: [
        { key: "connection", role: "alert", content: "Disconnected" },
        { key: "update", role: "status", content: "twr 1.2 is ready" },
        { key: "assistance", role: "status", content: "Suggestions paused" },
      ],
    });
    expect(html).toContain("Disconnected");
    expect(html).not.toContain("twr 1.2 is ready");
    expect(html).toContain('role="alert"');
    expect(html).toContain('aria-label="Show 2 more notices"');
    expect(html).toContain(">+2</button>");
  });
  it("shows nothing without notices, and no +N for one", () => {
    expect(render(StatusStrip, { items: [] })).toBe("");
    const one = render(StatusStrip, {
      items: [{ key: "update", role: "status", content: "twr 1.2 is ready" }],
    });
    expect(one).toContain("twr 1.2 is ready");
    expect(one).not.toContain("status-strip-more");
  });
});

describe("an actions menu", () => {
  it("names its button, marks toggles pressed and dialogs, and draws nothing empty", () => {
    const html = render(ActionsMenu, {
      label: "Conversation actions",
      items: [
        { key: "t", label: "Show tool calls", pressed: true, onSelect() {} },
        { key: "m", label: "Model & runtime…", dialog: true, onSelect() {} },
      ],
    });
    expect(html).toContain('aria-label="Conversation actions"');
    expect(html).toContain('aria-pressed="true"');
    expect(html).toContain('aria-haspopup="dialog"');
    expect(render(ActionsMenu, { label: "x", items: [] })).toBe("");
  });
});

describe("the thread inspector's actions", () => {
  const actions = (t: Thread) =>
    render(ThreadActions, {
      thread: t,
      has: all,
      pinned: false,
      onPin() {},
      action() {},
      more: [
        {
          key: "park",
          label: "Park…",
          hint: "Stop it and free its worktree; keep the branch.",
          onSelect() {},
        },
      ],
    });
  it("offers Stop while it works and Resume when parked", () => {
    for (const status of ["running", "background", "waiting"]) {
      const html = actions(thread({ status }));
      expect(html).toContain("Stop…");
      expect(html).not.toContain("Resume…");
    }
    const parked = actions(thread({ status: "parked" }));
    expect(parked).toContain("Resume…");
    expect(parked).not.toContain("Stop…");
    // The hub resumes only a parked thread; a finished one offers neither.
    const done = actions(thread({ status: "done" }));
    expect(done).not.toContain("Stop…");
    expect(done).not.toContain("Resume…");
  });
  it("keeps Pin and Rename in view, the rest under More with what each does", () => {
    const html = actions(thread());
    expect(html).toContain(">Pin</button>");
    expect(html).toContain("Rename…");
    expect(html).toContain(">More</summary>");
    expect(html).toContain("Stop it and free its worktree; keep the branch.");
  });
});

describe("the inspector's permission mode", () => {
  const picker = (props: Record<string, unknown>) =>
    render(PermissionModePicker, {
      id: "mode",
      thread: thread(),
      admin: true,
      editable: true,
      rpc: async () => ({}),
      fail() {},
      onChanged() {},
      ...props,
    });
  it("is a dropdown of modes in words", () => {
    const html = picker({});
    expect(html).toMatch(/^<select/);
    expect(html).toContain('<option value="auto" selected="">Agent decides');
    expect(html).toContain(">Ask nothing</option>");
  });
  it("offers only the narrow modes without admin, keeping the current one", () => {
    const html = picker({ admin: false });
    expect(html).not.toContain(">Ask nothing</option>");
    expect(html).toContain(">Plan only</option>");
    expect(html).toContain('value="auto" selected="">Agent decides');
  });
  it("reads as words where it can't be changed", () => {
    expect(picker({ editable: false })).toBe(
      '<b id="mode" class="inspector-mode">Agent decides</b>',
    );
  });
});

describe("search results", () => {
  it("say who an event came from", () => {
    expect(eventRole("turn.started")).toBe("You");
    expect(eventRole("turn.started", "user")).toBe("You");
    expect(eventRole("turn.started", "orchestrator")).toBe("Update");
    expect(eventRole("agent.text")).toBe("Agent");
    expect(eventRole("agent.tool_call")).toBe("Tool");
    expect(eventRole("agent.tool_result")).toBe("Tool");
    expect(eventRole("agent.thinking")).toBe("Reasoning");
    expect(eventRole("conversation.update")).toBe("Orchestrator");
    expect(eventRole("config.changed")).toBe("Update");
  });
  it("say when, the time alone today", () => {
    const now = new Date(2026, 10, 21, 15, 0);
    expect(
      eventTime(new Date(2026, 10, 21, 9, 5).toISOString(), now),
    ).not.toMatch(/Nov/);
    expect(eventTime(new Date(2026, 10, 2, 9, 5).toISOString(), now)).toMatch(
      /Nov/,
    );
    expect(eventTime("not a date", now)).toBe("");
  });
});
