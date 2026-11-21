import { describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  endedLoops,
  endedWords,
  kindWords,
  outcomeWords,
  phasesOf,
  reviewRequest,
  skipToVerify,
  startedNotice,
} from "../shared/loops";
import type { Thread } from "../shared/types";

vi.mock("../shared/host", () => ({ useHostAPI: () => ({}) }));

/** A single review pass (`once`): one review, the thread's triage, the report. */
describe("a single review pass", () => {
  it("asks loop.request for once from /review --once", () => {
    expect(reviewRequest({ once: true }, "t1")).toEqual({
      thread: "t1",
      kind: "review",
      once: true,
    });
    expect(reviewRequest({}, "t1")).toEqual({ thread: "t1", kind: "review" });
  });

  it("has no sweep, no verification and no skip to verify", () => {
    expect(phasesOf("review", true)).toEqual(["scope", "ladder", "report"]);
    expect(phasesOf("review")).toContain("verify");
    expect(skipToVerify({ phase: "ladder", once: true }).show).toBe(false);
    expect(skipToVerify({ phase: "ladder" }).show).toBe(true);
  });

  it("says it was a single pass when it ends", () => {
    expect(kindWords("review", true)).toBe("single review pass");
    expect(outcomeWords("triaged")).toBe("findings triaged");
    expect(
      endedWords({ id: "l1", kind: "review", outcome: "triaged", once: true }),
    ).toBe("single review pass findings triaged");
    expect(
      endedLoops(new Map([["t1", ""]]), [
        {
          id: "t1",
          name: "api",
          last_loop: {
            id: "l1",
            kind: "review",
            outcome: "clean",
            once: true,
          },
        },
      ]).said,
    ).toEqual(["api's single review pass ended: clean"]);
    expect(
      startedNotice("api", "review", "high · single pass · scope", true),
    ).toBe("api started a single review pass: high · single pass · scope.");
  });

  it("is offered beside the Review loop button and in the inspector", async () => {
    const { ReviewLoopOffer, LoopsSection } =
      await import("../shared/LoopViews");
    const thread = {
      id: "t1",
      name: "api",
      runtime: "claude-code",
      box_name: "local",
      status: "done",
      accent: "#fff",
      last_turn: { turn: 2 },
    } as unknown as Thread;
    const offer = renderToStaticMarkup(
      createElement(ReviewLoopOffer, { thread }),
    );
    expect(offer).toContain('role="group"');
    expect(offer).toContain('aria-label="Review once: api"');
    expect(offer).toContain('aria-label="Review loop: api"');
    expect(offer.indexOf("Review once")).toBeLessThan(
      offer.indexOf("Review loop<"),
    );
    const section = renderToStaticMarkup(
      createElement(LoopsSection, { connected: false, thread }),
    );
    expect(section).toMatch(/Review once<\/button>/);
    expect(section).toMatch(/Review loop<\/button>/);
  });
});
