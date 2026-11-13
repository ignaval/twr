import { describe, expect, it } from "vitest";
import {
  editSettings,
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
