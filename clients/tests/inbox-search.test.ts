import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { eventText, inboxCategory, NeedsYouInbox } from "../shared/InboxSearch";
import type { Attention, HubEvent, InboxSource } from "../shared/types";
vi.mock("../shared/host", () => ({
  useHostAPI: () => ({
    openExternal: async () => {},
    copyText: async () => {},
  }),
}));
const request = (reason: string, summary = "Request"): Attention => ({
  attn_id: reason,
  revision: 7,
  thread: "thread-1",
  reason,
  summary,
  options: ["allow", "deny"],
});
describe("Needs you grouping", () => {
  it("keeps a saved request when a workspace connection has not produced a live snapshot", () => {
    const source = {
      hubId: "local",
      hubName: "This computer",
      workspace: "default",
      attention: [],
      threads: [],
      cached: true,
      provisional: true,
    };
    const render = vi.fn(() => createElement("button", null, "Answer"));
    const html = renderToStaticMarkup(
      createElement(NeedsYouInbox, {
        source,
        sources: [
          {
            ...source,
            provisional: false,
            attention: [request("permission", "Keep the saved request")],
          },
        ],
        renderItem: render,
        onOpenSource: () => {},
      }),
    );
    expect(html).toContain("Keep the saved request");
    expect(html).not.toContain("No pending");
    expect(render).not.toHaveBeenCalled();
  });
  it("does not claim caught up when other workspace discovery failed", () => {
    const html = renderToStaticMarkup(
      createElement(NeedsYouInbox, {
        source: {
          hubId: "local",
          hubName: "This computer",
          workspace: "default",
          attention: [],
          threads: [],
        },
        sources: [
          {
            hubId: "__inbox_scan__",
            hubName: "Workspace scan",
            workspace: "",
            attention: [],
            threads: [],
            scanError: true,
            error:
              "The workspace scan could not finish. Some requests may be missing.",
          },
        ],
        renderItem: () => createElement("button", null, "Answer"),
        onOpenSource: () => {},
      }),
    );
    expect(html).toContain("Some requests may be missing");
    expect(html).not.toContain("You’re all caught up");
    expect(html).not.toContain("Open workspace</button>");
  });
  it("classifies only authoritative reasons, regardless of permission-like text", () => {
    expect(
      inboxCategory(request("review", "Permission requested: allow this")),
    ).toBe("completion");
    expect(inboxCategory(request("permission", "Finished work"))).toBe(
      "permission",
    );
    expect(inboxCategory(request("stalled", "Question for you"))).toBe("other");
    expect(inboxCategory(request("new-runtime-reason", "permission"))).toBe(
      "other",
    );
  });
  it("never renders response controls from the active workspace's saved summary", () => {
    const source: InboxSource = {
      hubId: "local",
      hubName: "This computer",
      workspace: "default",
      attention: [request("permission")],
      threads: [],
      cached: true,
    };
    const render = vi.fn(() => createElement("article", null, "ANSWER"));
    const html = renderToStaticMarkup(
      createElement(NeedsYouInbox, {
        source,
        renderItem: render,
        onOpenSource: () => {},
      }),
    );
    expect(render).not.toHaveBeenCalled();
    expect(html).toContain("Saved summary");
    expect(html).toContain("Open default");
    expect(html).not.toContain("data-inbox-key=");
  });
  it("offers a stopped workspace action without rendering an error or response controls", () => {
    const source: InboxSource = {
      hubId: "local",
      hubName: "This computer",
      workspace: "sleeping",
      attention: [request("permission")],
      threads: [],
      running: false,
    };
    const render = vi.fn(() => createElement("article", null, "ANSWER"));
    const html = renderToStaticMarkup(
      createElement(NeedsYouInbox, {
        source,
        renderItem: render,
        onOpenSource: () => {},
      }),
    );
    expect(render).not.toHaveBeenCalled();
    expect(html).toContain("Not running");
    expect(html).toContain("Open sleeping");
    expect(html).not.toContain("data-inbox-key=");
    const empty = renderToStaticMarkup(
      createElement(NeedsYouInbox, {
        source: { ...source, attention: [] },
        renderItem: render,
        onOpenSource: () => {},
      }),
    );
    expect(empty).toContain("Open workspace");
    expect(empty).not.toContain("No pending requests.");
    expect(empty).not.toContain('role="status"');
  });
  it("keeps equal workspace names on different hubs distinct and cached requests noninteractive", () => {
    const source: InboxSource = {
      hubId: "local",
      hubName: "This computer",
      workspace: "default",
      attention: [request("permission")],
      threads: [],
    };
    const other: InboxSource = {
      ...source,
      hubId: "remote",
      hubName: "Workstation",
      cached: true,
      attention: [request("question", "Remote request")],
    };
    const render = vi.fn((item: Attention) =>
      createElement("article", null, "Live " + item.revision),
    );
    const html = renderToStaticMarkup(
      createElement(NeedsYouInbox, {
        source,
        sources: [source, other],
        renderItem: render,
        onOpenSource: () => {},
      }),
    );
    expect(render).toHaveBeenCalledTimes(1);
    expect(html).toContain("Workstation");
    expect(html).toContain("This computer");
    expect(html).toContain("Saved summary");
    expect(html).toContain("Open default");
    expect(html).toContain('aria-label="Next request"');
    expect(html).toContain('data-inbox-key="permission:7"');
  });
});
describe("exact event rendering", () => {
  it("shows tool results and structured decisions under their own event identity", () => {
    const event: HubEvent = {
      id: "result",
      seq: 55,
      thread: "worker",
      kind: "agent.tool_result",
      ts: "2026-11-06T10:00:00Z",
      data: { output_summary: "Exact output\nline 2" },
    };
    expect(eventText(event)).toBe("Exact output\nline 2");
    expect(
      eventText({
        ...event,
        kind: "decision.made",
        data: { chosen: "deny", provider: "user" },
      }),
    ).toContain('"chosen": "deny"');
  });
});
