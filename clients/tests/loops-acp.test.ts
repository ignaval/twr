import { describe, expect, it } from "vitest";
import {
  acpReviewerNote,
  editSettings,
  reviewersSkipped,
  reviewerUsable,
  runtimesFor,
  type LoopSettings,
  type RuntimeInfo,
} from "../shared/loops";

// ACP reviewers (docs/design/loops.md, "ACP reviewers"): the editor's reviewer picker lists the
// ACP presets, enabled when they name a read-only mode and disabled with the hub's reason
// otherwise; the implementer's never does.
const runtimes: RuntimeInfo[] = [
  {
    runtime: "codex",
    found: true,
    login: true,
    models: ["gpt-6.1-sol"],
    efforts: ["medium", "xhigh"],
  },
  {
    runtime: "ex",
    found: true,
    login: true,
    probed: true,
    models: ["ex-large", "ex-small"],
    efforts: [],
    acp: true,
    review_mode: "read-only",
    cannot_review: null,
  },
  {
    runtime: "bare",
    found: true,
    login: true,
    models: [],
    efforts: [],
    acp: true,
    review_mode: null,
    cannot_review: "acp preset bare names no read-only mode",
  },
  {
    runtime: "gone",
    found: false,
    login: false,
    models: [],
    efforts: [],
    acp: true,
    review_mode: "plan",
    cannot_review: null,
  },
];

const settings: LoopSettings = {
  profile: "high",
  implementer: null,
  workers: { model: "claude-opus-5-5", effort: "high" },
  reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
  commit: "end",
  review_commit: "match",
  sweep: "thread",
} as LoopSettings;

describe("ACP reviewers in the settings editor", () => {
  it("offers presets that name a read-only mode, and says why the others can't review", () => {
    expect(reviewerUsable(runtimes[0])).toEqual({ usable: true });
    expect(reviewerUsable(runtimes[1])).toEqual({
      usable: true,
      note: "acp, read-only mode",
    });
    expect(reviewerUsable(runtimes[2])).toEqual({
      usable: false,
      why: "acp preset bare names no read-only mode",
    });
    expect(reviewerUsable(runtimes[3])).toEqual({
      usable: false,
      why: "not installed",
    });
  });

  it("lists ACP presets for the reviewer only", () => {
    expect(runtimesFor("reviewer", runtimes).map((r) => r.runtime)).toEqual([
      "codex",
      "ex",
      "bare",
      "gone",
    ]);
    expect(runtimesFor("implementer", runtimes).map((r) => r.runtime)).toEqual([
      "codex",
    ]);
  });

  it("picks a model the preset lists when the reviewer moves to it", () => {
    const next = editSettings(
      { settings, saveFor: "all" },
      { kind: "reviewer", runtime: "ex" },
      runtimes,
    );
    expect(next.settings.reviewer).toEqual({
      runtime: "ex",
      model: "ex-large",
    });
  });
});

describe("the reviewer picker's notes", () => {
  it("names every reviewer that can't be picked, with why, in words", () => {
    // A disabled option's reason shows only while its list is open, and dimmed.
    expect(reviewersSkipped(runtimes)).toBe(
      "Can't review: bare (acp preset bare names no read-only mode); gone (not installed)",
    );
    expect(reviewersSkipped(runtimes, "devbox")).toMatch(
      /^Can't review on box devbox: bare /,
    );
    expect(reviewersSkipped(runtimes.slice(0, 2))).toBeUndefined();
  });

  it("says what an ACP reviewer's read-only rests on and the efforts it takes", () => {
    const ex = { ...runtimes[1], efforts: ["medium", "xhigh"] };
    expect(acpReviewerNote(ex)).toBe(
      'ex is an ACP agent: it reviews in its own "read-only" mode, at the efforts medium, xhigh, which every round of the profile must use.',
    );
    expect(acpReviewerNote(runtimes[1])).toBe(
      'ex is an ACP agent: it reviews in its own "read-only" mode, at the effort its agent picks (its preset lists none).',
    );
    // Nothing for a built-in reviewer or a preset that can't review.
    expect(acpReviewerNote(runtimes[0])).toBeUndefined();
    expect(acpReviewerNote(runtimes[2])).toBeUndefined();
  });
});
