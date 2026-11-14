import { describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  SETUP_CANCEL,
  SETUP_START,
  editSettings,
  editorRequest,
  hasSubagents,
  loopAccess,
  loopSlash,
  matchingPreset,
  offerReviewSetting,
  offersReview,
  parseSetupSummary,
  reviewRequest,
  runtimeUsable,
  settingsLine,
  setupCardState,
  skippedPhases,
  startedWords,
  stoppedBySomeone,
  endedLoops,
  roundWords,
  waitingWords,
  type EditorState,
  type LoopSettings,
  type Preset,
  type RuntimeInfo,
  type SettingsGet,
} from "../shared/loops";
import { cardOf, attentionSummary } from "../shared/attentionSummary";
import { attentionSummarySchema } from "../desktop/validation";
import { kind, notices } from "../desktop/notify-state";
import { inboxCategory } from "../shared/InboxSearch";
import type { Attention, Thread } from "../shared/types";

vi.mock("../shared/host", () => ({ useHostAPI: () => ({}) }));

const worker = (extra: Partial<Thread> = {}): Thread =>
  ({
    id: "t1",
    name: "api",
    runtime: "claude-code",
    box_name: "local",
    mode: "auto",
    status: "done",
    accent: "#fff",
    is_orchestrator: false,
    parked: false,
    stopped: false,
    current_turn: null,
    last_turn: { turn: 4, result: "completed" },
    git: { branch: "twr/api", dirty_files: 3, ahead: 0, behind: 0 },
    ...extra,
  }) as Thread;

describe("the Review loop offer", () => {
  it("offers a review of a finished turn that left changes", () => {
    expect(offersReview(worker(), true)).toBe(true);
    expect(offersReview(worker(), undefined)).toBe(true);
    // Commits ahead of its base count as changes too.
    expect(
      offersReview(
        worker({ git: { dirty_files: 0, ahead: 0, base_ahead: 2 } }),
        true,
      ),
    ).toBe(true);
  });

  it("is off when [loops] offer_review is false", () => {
    expect(offersReview(worker(), false)).toBe(false);
    expect(offerReviewSetting({ loops: { offer_review: false } })).toBe(false);
    expect(offerReviewSetting({ loops: { offer_review: true } })).toBe(true);
    // An older hub says nothing about loops: the button shows.
    expect(offerReviewSetting({})).toBe(true);
    expect(offerReviewSetting(undefined)).toBe(true);
  });

  it("never offers on the orchestrator, a parked thread, or a thread at work", () => {
    expect(offersReview(worker({ is_orchestrator: true }), true)).toBe(false);
    expect(offersReview(worker({ parked: true }), true)).toBe(false);
    expect(offersReview(worker({ status: "parked" }), true)).toBe(false);
    expect(offersReview(worker({ current_turn: 5 }), true)).toBe(false);
    expect(
      offersReview(worker({ loop: { id: "l1", phase: "ladder" } }), true),
    ).toBe(false);
    expect(offersReview(undefined, true)).toBe(false);
  });

  it("offers on a thread stopped only by the hub's shutdown, not on one someone stopped", () => {
    // The hub's shutdown stops every thread, and a loop on one goes on once it is sent
    // something; a thread someone stopped, the hub refuses until it is sent something.
    expect(
      offersReview(worker({ stopped: true, stopped_by_hub: true }), true),
    ).toBe(true);
    expect(offersReview(worker({ stopped: true }), true)).toBe(false);
    expect(
      offersReview(worker({ stopped: true, stopped_by_hub: false }), true),
    ).toBe(false);
    expect(stoppedBySomeone(worker({ stopped: true }))).toBe(true);
    expect(
      stoppedBySomeone(worker({ stopped: true, stopped_by_hub: true })),
    ).toBe(false);
    expect(stoppedBySomeone(worker())).toBe(false);
  });

  it("needs a finished turn and changes in its git state", () => {
    expect(offersReview(worker({ last_turn: undefined }), true)).toBe(false);
    expect(
      offersReview(
        worker({ git: { dirty_files: 0, ahead: 3, base_ahead: 0 } }),
        true,
      ),
    ).toBe(false);
    expect(offersReview(worker({ git: undefined }), true)).toBe(false);
  });

  it("goes away once a loop reviewed the last turn, and comes back after a newer one", () => {
    const reviewed = {
      id: "l1",
      kind: "review",
      outcome: "clean",
      ended_turn: 4,
    };
    expect(offersReview(worker({ last_loop: reviewed }), true)).toBe(false);
    expect(
      offersReview(
        worker({ last_loop: reviewed, last_turn: { turn: 5 } }),
        true,
      ),
    ).toBe(true);
    expect(
      offersReview(
        worker({ last_loop: { ...reviewed, ended_turn: null } }),
        true,
      ),
    ).toBe(true);
  });
});

// As the hub's `card_text` writes it.
const summary = [
  "api: review loop setup (first run: choose its models). Start and save these settings?",
  "Profile      high · medium tier, then xhigh tier · sweep · 5 verify",
  "Scope        branch · 7 commits, 23 files",
  "Implementer  claude-code · claude-opus-5-5 · high (the thread's own)",
  "Workers      claude-opus-5-5 · high",
  "Reviewer     codex · gpt-6.1-sol · medium → xhigh",
  "Save for     all projects",
  "Warning: codex isn't installed: reviewing with claude-code · claude-opus-5-5 instead",
  "Warning: reviewer and implementer are the same family: it tends to miss what the implementer missed",
].join("\n");

describe("the setup card", () => {
  it("reads its title, rows and warnings", () => {
    const parsed = parseSetupSummary(summary);
    expect(parsed.title).toMatch(/^api: review loop setup/);
    expect(parsed.rows).toEqual([
      ["Profile", "high · medium tier, then xhigh tier · sweep · 5 verify"],
      ["Scope", "branch · 7 commits, 23 files"],
      [
        "Implementer",
        "claude-code · claude-opus-5-5 · high (the thread's own)",
      ],
      ["Workers", "claude-opus-5-5 · high"],
      ["Reviewer", "codex · gpt-6.1-sol · medium → xhigh"],
      ["Save for", "all projects"],
    ]);
    expect(parsed.warnings).toHaveLength(2);
    expect(parsed.problems).toEqual([]);
    expect(parsed.other).toEqual([]);
  });

  it("keeps what it can't place, and reads Change first lines", () => {
    const parsed = parseSetupSummary(
      "Title line\nsomething odd\nChange first: grok-build is not logged in\n\nThis run     profile low",
    );
    expect(parsed.other).toEqual(["something odd"]);
    expect(parsed.problems).toEqual(["grok-build is not logged in"]);
    expect(parsed.rows).toEqual([["This run", "profile low"]]);
    expect(parseSetupSummary("")).toEqual({
      title: "",
      rows: [],
      warnings: [],
      problems: [],
      other: [],
    });
  });

  it("starts only when the card offers it and lists nothing to change first", () => {
    const item = {
      loop_setup: true,
      summary,
      options: [SETUP_START, SETUP_CANCEL],
    };
    expect(setupCardState(item)).toMatchObject({
      canStart: true,
      canCancel: true,
    });
    expect(
      setupCardState({
        ...item,
        summary: summary + "\nChange first: codex is not logged in",
      }).canStart,
    ).toBe(false);
    expect(setupCardState({ ...item, options: [SETUP_CANCEL] }).canStart).toBe(
      false,
    );
  });

  it("counts as needing you in the tray and notifications", () => {
    const card = {
      attn_id: "a1",
      revision: 3,
      thread: "t1",
      reason: "review",
      summary,
      options: [SETUP_START, SETUP_CANCEL],
      loop_setup: true,
    } as Attention;
    const stuck = {
      ...card,
      attn_id: "a2",
      reason: "error",
      summary: "api's loop is stuck",
      options: ["resume", "stop"],
      loop_setup: undefined,
      loop_stuck: true,
    } as Attention;
    expect(cardOf(card)).toBe("loop_setup");
    expect(cardOf(stuck)).toBe("loop_stuck");
    expect(cardOf({ ...card, loop_setup: false })).toBeUndefined();
    const result = attentionSummary({
      current: "default",
      hubId: "local",
      live: { threads: [worker()], attention: [card, stuck] },
      liveStream: "live:1",
      sources: [],
    });
    expect(result.items.map((i) => i.card)).toEqual([
      "loop_setup",
      "loop_stuck",
    ]);
    // The desktop's main process takes it as it is.
    expect(attentionSummarySchema.safeParse(result).success).toBe(true);
    expect(notices(result.items)).toHaveLength(2);
    expect(kind(result.items[0])).toBe("Asks you to choose a loop's settings");
    expect(kind(result.items[1])).toBe("Has a loop that is stuck");
    expect(inboxCategory(card)).toBe("question");
    expect(inboxCategory(stuck)).toBe("question");
  });
});

const runtimes: RuntimeInfo[] = [
  {
    runtime: "claude-code",
    found: true,
    login: true,
    models: ["claude-opus-5-5", "claude-sonnet-5-5"],
    efforts: ["low", "medium", "high", "xhigh"],
  },
  {
    runtime: "codex",
    found: true,
    login: true,
    models: ["gpt-6.1-sol", "gpt-6.1-mini"],
    efforts: ["low", "medium", "high", "xhigh"],
    accounts: ["work", "home"],
  },
  {
    runtime: "grok-build",
    found: false,
    login: false,
    models: ["grok-4.7"],
    efforts: ["high"],
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
};
const presets: Preset[] = [
  {
    id: "claude-code+codex",
    label: "Claude Code + Codex",
    implementer: {
      runtime: "claude-code",
      model: "claude-opus-5-5",
      effort: "high",
    },
    workers: { model: "claude-opus-5-5", effort: "high" },
    reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
  },
  {
    id: "codex",
    label: "all Codex",
    implementer: { runtime: "codex", model: "gpt-6.1-sol", effort: "high" },
    workers: { model: "gpt-6.1-sol", effort: "high" },
    reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
  },
];
const start: EditorState = { settings, saveFor: "all", revision: 7 };

describe("the settings editor", () => {
  it("applies a preset's implementer, workers and reviewer, and nothing else", () => {
    const next = editSettings(start, { kind: "preset", preset: presets[1] });
    expect(next.settings).toEqual({
      ...settings,
      implementer: presets[1].implementer,
      workers: presets[1].workers,
      reviewer: presets[1].reviewer,
    });
    expect(matchingPreset(next.settings, presets)).toBe("codex");
    // The thread's own implementer matches no preset.
    expect(matchingPreset(settings, presets)).toBeUndefined();
  });

  it("names an implementer and goes back to the thread's own", () => {
    let next = editSettings(
      start,
      { kind: "implementer", runtime: "claude-code" },
      runtimes,
    );
    expect(next.settings.implementer).toEqual({
      runtime: "claude-code",
      model: "claude-opus-5-5",
    });
    next = editSettings(
      next,
      { kind: "implementer", model: "claude-sonnet-5-5", effort: "xhigh" },
      runtimes,
    );
    expect(next.settings.implementer).toEqual({
      runtime: "claude-code",
      model: "claude-sonnet-5-5",
      effort: "xhigh",
    });
    // Another runtime: its first model, the runtime's default effort.
    next = editSettings(
      next,
      { kind: "implementer", runtime: "codex" },
      runtimes,
    );
    expect(next.settings.implementer).toEqual({
      runtime: "codex",
      model: "gpt-6.1-sol",
    });
    // An empty effort is the runtime's default.
    next = editSettings(
      next,
      { kind: "implementer", effort: "high" },
      runtimes,
    );
    next = editSettings(next, { kind: "implementer", effort: "" }, runtimes);
    expect(next.settings.implementer).toEqual({
      runtime: "codex",
      model: "gpt-6.1-sol",
    });
    next = editSettings(next, { kind: "implementer", own: true }, runtimes);
    expect(next.settings.implementer).toBeNull();
  });

  it("changes workers, reviewer, profile, commits and where it saves", () => {
    let next = editSettings(start, { kind: "workers", effort: "" });
    expect(next.settings.workers).toEqual({ model: "claude-opus-5-5" });
    next = editSettings(next, { kind: "workers", model: "claude-sonnet-5-5" });
    expect(next.settings.workers).toEqual({ model: "claude-sonnet-5-5" });
    next = editSettings(next, { kind: "reviewer", account: "work" }, runtimes);
    expect(next.settings.reviewer).toEqual({
      runtime: "codex",
      account: "work",
      model: "gpt-6.1-sol",
    });
    // Another runtime drops the account and takes its first model.
    next = editSettings(
      next,
      { kind: "reviewer", runtime: "claude-code" },
      runtimes,
    );
    expect(next.settings.reviewer).toEqual({
      runtime: "claude-code",
      model: "claude-opus-5-5",
    });
    next = editSettings(next, { kind: "profile", profile: "low" });
    next = editSettings(next, { kind: "review_commit", value: "none" });
    next = editSettings(next, { kind: "save_for", value: "project" });
    expect(next.settings.profile).toBe("low");
    expect(next.settings.review_commit).toBe("none");
    expect(next.saveFor).toBe("project");
    // Untouched: what the editor doesn't show.
    expect(next.settings.commit).toBe("end");
    expect(next.settings.sweep).toBe("thread");
    expect("plan_check" in next.settings).toBe(false);
    // The implementation-only rows: a plan check of its own, then the profile's again.
    next = editSettings(next, { kind: "plan_check", value: "always" });
    next = editSettings(next, { kind: "commit", value: "per-round" });
    expect(next.settings.plan_check).toBe("always");
    expect(next.settings.commit).toBe("per-round");
    next = editSettings(next, { kind: "plan_check", value: "" });
    expect("plan_check" in next.settings).toBe(false);
    expect(
      JSON.parse(JSON.stringify(next.settings)).plan_check,
    ).toBeUndefined();
    expect(next.revision).toBe(7);
  });

  it("sends a card's draft at the revision shown, and saved settings for their project", () => {
    expect(
      editorRequest({ kind: "card", attnId: "a1", project: "api" }, start),
    ).toEqual({
      method: "loop.setup.update",
      params: {
        attn_id: "a1",
        revision: 7,
        settings,
        save_for: "all",
      },
    });
    expect(editorRequest({ kind: "saved" }, start)).toEqual({
      method: "loop.settings.set",
      params: { settings },
    });
    expect(
      editorRequest({ kind: "saved", project: "api" }, { ...start }),
    ).toEqual({ method: "loop.settings.set", params: { settings } });
    expect(
      editorRequest(
        { kind: "saved", project: "api" },
        { ...start, saveFor: "project" },
      ),
    ).toEqual({
      method: "loop.settings.set",
      params: { project: "api", settings },
    });
  });

  it("offers only runtimes that are found and logged in, and workers where there are subagents", () => {
    expect(runtimeUsable(runtimes[0])).toEqual({ usable: true });
    expect(runtimeUsable(runtimes[2])).toEqual({
      usable: false,
      why: "not installed",
    });
    expect(runtimeUsable({ ...runtimes[1], login: false })).toEqual({
      usable: false,
      why: "not logged in",
    });
    expect(runtimeUsable(undefined).usable).toBe(false);
    // A hub that does not probe its agent CLIs doesn't know: usable, and said so.
    expect(
      runtimeUsable({
        ...runtimes[2],
        found: true,
        login: true,
        probed: false,
      }),
    ).toEqual({ usable: true, note: "not checked" });
    expect(hasSubagents("claude-code", runtimes)).toBe(true);
    expect(hasSubagents("codex", runtimes)).toBe(false);
    expect(hasSubagents("codex", [{ ...runtimes[1], subagents: true }])).toBe(
      true,
    );
    expect(hasSubagents(undefined, runtimes)).toBe(false);
  });
});

describe("what a loop started on", () => {
  const started = {
    settings: {
      ...settings,
      profile: "low",
      implementer: null,
      reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
    },
    settings_from: "saved",
    settings_of: "workspace",
  };
  const api = worker({ model_actual: "fake-1", project: "demo" });
  it("names the models and whose settings they are, as the hub's line does", () => {
    expect(startedWords(started, api)).toBe(
      "low · implementer fake-1 (the thread's own) · reviewer codex gpt-6.1-sol (saved settings)",
    );
    expect(startedWords({ ...started, settings_of: "project" }, api)).toMatch(
      /\(demo's settings\)$/,
    );
    expect(
      startedWords(
        {
          ...started,
          settings_from: "setup_item",
          settings_of: null,
          settings: {
            ...started.settings,
            implementer: {
              runtime: "claude-code",
              model: "claude-opus-5-5",
              effort: "high",
            },
          },
        },
        api,
      ),
    ).toBe(
      "low · implementer claude-opus-5-5 high · reviewer codex gpt-6.1-sol (settings chosen on its setup card)",
    );
  });
  it("says nothing when the hub said nothing", () => {
    expect(startedWords(undefined, api)).toBe("");
    expect(startedWords({}, api)).toBe("");
  });
});

describe("what a loop would run on, and where it is", () => {
  const got: SettingsGet = {
    settings: {
      ...settings,
      implementer: {
        runtime: "claude-code",
        model: "claude-opus-5-5",
        effort: "high",
      },
    },
    source: "project",
    first_card: settings,
    projects: ["api"],
    presets,
    profiles: ["low", "medium", "high"],
    runtimes,
    warnings: [],
    problems: [],
  };
  it("says the profile, implementer and reviewer, or that the first run asks", () => {
    expect(settingsLine(got)).toBe(
      "high · implementer claude-code · claude-opus-5-5 · high · reviewer codex · gpt-6.1-sol (this project's settings)",
    );
    expect(
      settingsLine({
        ...got,
        source: "workspace",
        settings: { ...settings, implementer: null },
      }),
    ).toBe(
      "high · implementer the thread's own model · reviewer codex · gpt-6.1-sol (saved settings)",
    );
    expect(settingsLine({ ...got, settings: null, source: null })).toBe(
      "first run: you will be asked to choose models",
    );
  });
  it("says a round in flight is under review, not clean", () => {
    const rounds = [
      { id: "m1", findings: 1, new_class: 0 },
      { id: "m2", findings: 0, new_class: 0 },
    ];
    const reviewing = { rounds, waiting_on: "review" as const };
    expect(roundWords(rounds[1], reviewing)).toBe("under review");
    expect(roundWords(rounds[0], reviewing)).toBe("1 finding · 0 new class");
    expect(roundWords(rounds[1], { rounds, waiting_on: "thread" })).toBe(
      "0 findings · 0 new class",
    );
    expect(roundWords({ ...rounds[1], failed: true }, reviewing)).toBe(
      "review run failed",
    );
  });
  it("says what a running loop waits on", () => {
    expect(
      waitingWords({ waiting_on: "review", paused: false, stuck: null }),
    ).toBe("waiting on the review");
    // Paused during a review run: the status says so, not only the Resume button.
    expect(
      waitingWords({ waiting_on: "review", paused: true, stuck: null }),
    ).toBe("paused: the review in flight finishes first");
    expect(
      waitingWords({ waiting_on: "thread", paused: false, stuck: null }),
    ).toBe("waiting on the thread");
    expect(
      waitingWords({ waiting_on: "user", paused: true, stuck: null }),
    ).toBe("paused: waiting on you");
    expect(
      waitingWords({ waiting_on: "user", paused: true, stuck: "triage m3" }),
    ).toBe("stuck: triage m3");
  });

  it("marks a sweep and a verification the loop passed over as skipped", () => {
    const base = {
      id: "l1",
      kind: "review",
      profile: "high",
      rounds: [],
      ledger: 0,
      verify: { done: 0, cap: 5 },
      paused: false,
      waiting_on: "thread" as const,
      reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
      plan: { sweep: true },
    };
    // In the ladder: nothing passed over yet.
    expect(skippedPhases({ ...base, phase: "ladder", fixed: 0 })).toEqual([]);
    // The last tier's empty round: no sweep (nothing fixed), the verification skipped.
    expect(
      skippedPhases({
        ...base,
        phase: "report",
        fixed: 0,
        transition: { reason: "converged", round: "x1", verify_skipped: true },
      }),
    ).toEqual(["sweep", "verify"]);
    // Something fixed: the sweep ran, and so does the verification.
    expect(
      skippedPhases({
        ...base,
        phase: "verify",
        fixed: 2,
        transition: { reason: "converged", round: "x1" },
      }),
    ).toEqual([]);
    expect(
      skippedPhases({
        ...base,
        phase: "verify",
        fixed: 2,
        transition: { reason: "skipped", round: "m1" },
      }),
    ).toEqual(["sweep"]);
  });
});

describe("a loop that ends", () => {
  it("is said once, for any thread, and not on first sight", () => {
    const api = { id: "t1", name: "api", last_loop: undefined };
    const first = endedLoops(undefined, [
      { ...api, last_loop: { id: "l0", kind: "review", outcome: "clean" } },
    ]);
    expect(first.said).toEqual([]);
    const ended = endedLoops(first.known, [
      { ...api, last_loop: { id: "l1", kind: "review", outcome: "stopped" } },
      { id: "t2", name: "web", last_loop: { id: "l9", kind: "review" } },
    ]);
    expect(ended.said).toEqual(["api's review loop ended: stopped"]);
    expect(
      endedLoops(ended.known, [
        { ...api, last_loop: { id: "l1", kind: "review", outcome: "stopped" } },
      ]).said,
    ).toEqual([]);
  });
});

describe("who may act on loops", () => {
  it("needs operate and a live connection", () => {
    expect(loopAccess({ connected: true })).toEqual({ act: true });
    expect(
      loopAccess({ scopes: ["read", "operate"], connected: true }),
    ).toEqual({ act: true });
    expect(loopAccess({ scopes: ["read"], connected: true })).toEqual({
      act: false,
      why: "This device has read-only access.",
    });
    expect(
      loopAccess({ scopes: ["read", "operate"], connected: false }).act,
    ).toBe(false);
  });
});

describe("the slash commands", () => {
  it("maps /review to loop.request on the named or chosen thread", () => {
    expect(reviewRequest({ setup: false }, "t1")).toEqual({
      thread: "t1",
      kind: "review",
    });
    expect(
      reviewRequest(
        {
          thread: "api",
          setup: true,
          scope: "since main",
          profile: "low",
          reviewer: "codex/gpt-6.1-sol",
          implementer: "",
        },
        "t1",
      ),
    ).toEqual({
      thread: "api",
      kind: "review",
      setup: true,
      scope: "since main",
      overrides: { profile: "low", reviewer: "codex/gpt-6.1-sol" },
    });
    expect(() => reviewRequest({ setup: false }, undefined)).toThrow(
      /Choose a thread/,
    );
  });
  it("maps /loop to a control, the editor, or the loop's progress", () => {
    expect(loopSlash({ action: "pause" }, "t1")).toEqual({
      kind: "control",
      thread: "t1",
      action: "pause",
    });
    expect(
      loopSlash({ thread: "api", action: "skip_to_verify" }, "t1"),
    ).toEqual({ kind: "control", thread: "api", action: "skip_to_verify" });
    expect(
      loopSlash({ action: "settings", project: "api" }, undefined),
    ).toEqual({ kind: "saved", project: "api" });
    expect(loopSlash({ action: "settings" }, "t1")).toEqual({ kind: "saved" });
    expect(loopSlash({ action: "status" }, "t1")).toEqual({
      kind: "status",
      thread: "t1",
    });
    expect(() => loopSlash({ action: "stop" }, undefined)).toThrow(
      /Choose a thread/,
    );
  });
});

describe("rendered", () => {
  (globalThis as any).window ??= { twr: {} };
  it("draws the chip from the hub's badge and line only", async () => {
    const { LoopChip } = await import("../shared/LoopViews");
    expect(
      renderToStaticMarkup(createElement(LoopChip, { thread: worker() })),
    ).toBe("");
    const html = renderToStaticMarkup(
      createElement(LoopChip, {
        thread: worker({
          loop: { id: "l1" },
          loop_badge: "REVIEW LOOP",
          loop_line: "high · ladder m3 · ledger 2",
        }),
      }),
    );
    expect(html).toContain("REVIEW LOOP");
    expect(html).toContain("high · ladder m3 · ledger 2");
  });
  it("draws the setup card's rows, and holds Start while something has to change first", async () => {
    const { LoopSetupCard } = await import("../shared/LoopViews");
    const item = {
      attn_id: "a1",
      revision: 3,
      thread: "t1",
      reason: "review",
      summary: summary + "\nChange first: codex is not logged in",
      options: [SETUP_START, SETUP_CANCEL],
      loop_setup: true,
    } as Attention;
    const html = renderToStaticMarkup(
      createElement(LoopSetupCard, {
        item,
        thread: worker(),
        readOnly: false,
        onRespond: async () => {},
      }),
    );
    expect(html).toContain(
      "<dt>Reviewer</dt><dd>codex · gpt-6.1-sol · medium → xhigh</dd>",
    );
    expect(html).toContain("loop-warning");
    expect(html).toContain("loop-problem");
    expect(html).toMatch(
      /<button[^>]*disabled=""[^>]*>Start and save<\/button>/,
    );
    expect(html).not.toContain("Dismiss");
    // The inspector focuses a card it just raised: it takes focus, and says whose it is.
    expect(html).toContain('data-thread="t1"');
    expect(html).toContain('tabindex="-1"');
  });
  it("shows the round in flight as under review, and a pause during it", async () => {
    const { LoopsSection } = await import("../shared/LoopViews");
    const running = (paused: boolean) =>
      renderToStaticMarkup(
        createElement(LoopsSection, {
          connected: false,
          thread: worker({
            loop: {
              id: "l1",
              kind: "review",
              profile: "low",
              phase: "ladder",
              round: "m1",
              rounds: [{ id: "m1", findings: 0, new_class: 0 }],
              ledger: 0,
              verify: { done: 0, cap: 1 },
              paused,
              waiting_on: "review",
              reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
            },
          }),
        }),
      );
    const html = running(false);
    expect(html).toContain("<b>m1</b> under review");
    expect(html).not.toContain("0 findings");
    expect(html).toContain("waiting on the review");
    expect(html).toContain("0 of 1 rounds done");
    expect(html).toContain('aria-label="Loop controls"');
    expect(running(true)).toContain(
      "paused: the review in flight finishes first",
    );
  });
  it("leaves Start to the 600 ms rule when the card lists nothing to change", async () => {
    const { LoopSetupCard } = await import("../shared/LoopViews");
    const html = renderToStaticMarkup(
      createElement(LoopSetupCard, {
        item: {
          attn_id: "a1",
          revision: 3,
          thread: "t1",
          reason: "review",
          summary,
          options: [SETUP_START, SETUP_CANCEL],
          loop_setup: true,
        } as Attention,
        thread: worker(),
        readOnly: false,
        onRespond: async () => {},
      }),
    );
    // Not yet on screen for 600 ms: shown as not available, not disabled.
    expect(html).toMatch(
      /<button type="button" class="primary" aria-disabled="true">Start and save<\/button>/,
    );
  });
  it("is a row that opens a sheet on a phone", async () => {
    const { LoopSetupCard } = await import("../shared/LoopViews");
    const g = globalThis as any;
    const saved = { document: g.document, matchMedia: g.window.matchMedia };
    g.document = { documentElement: { dataset: { mobile: "true" } } };
    g.window.matchMedia = () => ({ matches: true });
    try {
      const html = renderToStaticMarkup(
        createElement(LoopSetupCard, {
          item: {
            attn_id: "a1",
            revision: 3,
            thread: "t1",
            reason: "review",
            summary,
            options: [SETUP_START, SETUP_CANCEL],
            loop_setup: true,
          } as Attention,
          thread: worker(),
          readOnly: false,
          onRespond: async () => {},
        }),
      );
      expect(html).toContain(
        'class="attention-card attention-loop-setup loop-setup-row"',
      );
      expect(html).toMatch(
        /<button type="button" class="primary" aria-haspopup="dialog" aria-expanded="false">Review setup<\/button>/,
      );
      expect(html).not.toContain(">Start and save</button>");
    } finally {
      g.document = saved.document;
      g.window.matchMedia = saved.matchMedia;
    }
  });
});
