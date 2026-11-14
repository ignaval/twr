import { describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { Thread } from "../shared/types";
import {
  auditWords,
  auditingWords,
  editSettings,
  waitingWords,
  type AuditView,
  type EditorState,
} from "../shared/loops";

vi.mock("../shared/host", () => ({ useHostAPI: () => ({}) }));

/** The sweep audited by runs (`sweep = "runs"`, docs/design/loops.md, milestone 4). */
describe("the sweep's audit runs", () => {
  const audits: AuditView[] = [
    { rule: 1, name: "a", status: "answered", findings: 2 },
    { rule: 2, name: "b", status: "running", findings: 0 },
    { rule: 3, name: "c", status: "running", findings: 0, retried: true },
    { rule: 4, name: "d", status: "not_run", findings: 0, reason: "timed out" },
    { rule: 5, name: "e", status: "pending", findings: 0 },
  ];

  it("says what each audit found or where it is", () => {
    expect(audits.map(auditWords)).toEqual([
      "2 findings",
      "auditing",
      "auditing again (its run failed once)",
      "not run: timed out",
      "waiting for a slot",
    ]);
    expect(
      auditWords({ rule: 1, name: "a", status: "answered", findings: 0 }),
    ).toBe("no place breaks it");
  });

  it("tells the card how many rules are being audited", () => {
    const thread = {
      loop: { wait: { step: "audits" }, audits },
      background_tasks: {
        "loop:l1:s2": { description: "audit s2 b", review: true },
        "loop:l1:s3": { description: "audit s3 c", review: true },
      },
    };
    expect(auditingWords(thread)).toBe("auditing 2 of 5 rules");
    // Other background work of the thread's: the card says what each does instead.
    expect(
      auditingWords({
        ...thread,
        background_tasks: {
          ...thread.background_tasks,
          "job:1": { description: "job cargo test" },
        },
      }),
    ).toBeUndefined();
    expect(
      auditingWords({
        loop: { wait: { step: "review" } },
        background_tasks: {},
      }),
    ).toBeUndefined();
  });

  it("says the loop waits on its audits, paused or not", () => {
    const l = {
      waiting_on: "review" as const,
      stuck: null,
      wait: { step: "audits" },
    };
    expect(waitingWords({ ...l, paused: false })).toBe("waiting on the audits");
    expect(waitingWords({ ...l, paused: true })).toBe(
      "paused: the audits in flight finish first",
    );
  });

  it("switches the editor's sweep", () => {
    const state: EditorState = {
      settings: {
        profile: "high",
        workers: { model: "claude-opus-5-5" },
        reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
      },
      saveFor: "all",
    };
    const next = editSettings(state, { kind: "sweep", value: "runs" });
    expect(next.settings.sweep).toBe("runs");
    expect(state.settings.sweep).toBeUndefined();
  });
});

describe("the inspector's sweep audits", () => {
  it("lists each rule's audit like a round", async () => {
    const { LoopsSection } = await import("../shared/LoopViews");
    const thread = {
      id: "t1",
      name: "api",
      project: "api",
      runtime: "claude-code",
      box_name: "local",
      mode: "auto",
      status: "background",
      accent: "#fff",
      is_orchestrator: false,
      current_turn: null,
      loop_badge: "REVIEW LOOP",
      loop: {
        id: "l1",
        kind: "review",
        profile: "medium",
        phase: "sweep",
        rounds: [{ id: "m1", findings: 1, new_class: 1 }],
        ledger: 0,
        verify: { done: 0, cap: 2 },
        paused: false,
        waiting_on: "review",
        wait: { step: "audits" },
        reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
        plan: { sweep: true },
        audits: [
          { rule: 1, name: "identity-gating", status: "answered", findings: 2 },
          {
            rule: 2,
            name: "retries",
            status: "not_run",
            findings: 0,
            reason: "timed out",
          },
        ],
      },
    } as unknown as Thread;
    const html = renderToStaticMarkup(
      createElement(LoopsSection, { connected: false, thread }),
    );
    expect(html).toContain('aria-label="Sweep audits"');
    expect(html).toContain("<b>s1</b> identity-gating · 2 findings");
    expect(html).toContain("<b>s2</b> retries · not run: timed out");
    expect(html).toContain("waiting on the audits");
  });
});
