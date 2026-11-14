import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
vi.mock("electron", () => ({ Notification: { isSupported: () => true } }));
vi.mock("../shared/host", () => ({ useHostAPI: () => ({}) }));
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  SETUP_CANCEL,
  SETUP_RUN_WITHOUT,
  SETUP_START,
  commitNotes,
  commitWords,
  countedRounds,
  endedLately,
  endedLoops,
  endedWords,
  implementRequest,
  implementTarget,
  implementedWords,
  newThreadLoop,
  newThreadRequest,
  newThreadValues,
  offersReview,
  parseSetupSummary,
  planWords,
  sendAsLoop,
  setupCardState,
  setupOptions,
  skipToVerify,
  skippedPhases,
  startedNotice,
  type LoopState,
} from "../shared/loops";
import { attentionSummary } from "../shared/attentionSummary";
import { attentionSummarySchema } from "../desktop/validation";
import { LoopWatch, isLoopNotice, loopNotices } from "../desktop/notify-state";
import { Notifier, type Post } from "../desktop/notifier";
import { commands, parameters } from "../shared/commands";
import type {
  Attention,
  AttentionSummary,
  LoopEndSummary,
  Thread,
} from "../shared/types";

/* Implementation loops in the apps (docs/design/loops.md › Milestone 2): Send as
 * implementation loop, /implement, the New thread form's switch, the setup card that holds a
 * task, the inspector's phases, the ended line and the notification of a loop's end. */

const worker = (extra: Partial<Thread> = {}): Thread =>
  ({
    id: "t1",
    name: "api",
    project: "api",
    runtime: "claude-code",
    box_name: "local",
    mode: "auto",
    status: "done",
    accent: "#fff",
    is_orchestrator: false,
    current_turn: null,
    last_turn: { turn: 4, result: "completed" },
    git: { dirty_files: 2 },
    ...extra,
  }) as Thread;

/** The hub's card for an implementation loop that holds a task (`card_text`). */
const implementCard = [
  "api: implementation loop setup (first run: choose its models). Start and save these settings?",
  "Profile      high · medium tier, then xhigh tier · sweep · 5 verify",
  "Task         add a CSV export to the reports page",
  "             keep the  columns as they are",
  "             test it with the fixtures",
  "             (+2 more lines)",
  "Baseline     clean",
  "Implementer  claude-code · claude-opus-5-5 · high (the thread's own)",
  "Workers      claude-opus-5-5 · high",
  "Reviewer     codex · gpt-6.1-sol · medium → xhigh",
  "Plan check   contracts (the profile's)",
  "Commit       end",
  "Save for     all projects",
].join("\n");
const card = (extra: Partial<Attention> = {}): Attention =>
  ({
    attn_id: "a1",
    revision: 3,
    thread: "t1",
    reason: "review",
    summary: implementCard,
    options: [SETUP_START, SETUP_RUN_WITHOUT, SETUP_CANCEL],
    loop_setup: true,
    ...extra,
  }) as Attention;

describe("the setup card of an implementation loop", () => {
  it("reads the held task's lines as one row, and the plan check and commit rows", () => {
    const parsed = parseSetupSummary(implementCard);
    const rows = Object.fromEntries(parsed.rows);
    // An indented line goes on with the row before it, even with two spaces inside.
    expect(rows.Task).toBe(
      "add a CSV export to the reports page\nkeep the  columns as they are\ntest it with the fixtures\n(+2 more lines)",
    );
    expect(rows["Plan check"]).toBe("contracts (the profile's)");
    expect(rows.Commit).toBe("end");
    expect(rows.Baseline).toBe("clean");
    expect(parsed.other).toEqual([]);
    expect(parsed.rows.map(([label]) => label)).toEqual([
      "Profile",
      "Task",
      "Baseline",
      "Implementer",
      "Workers",
      "Reviewer",
      "Plan check",
      "Commit",
      "Save for",
    ]);
  });

  it("says what kind of loop it is for, and has three answers in the hub's order", () => {
    const state = setupCardState(card());
    expect(state.kind).toBe("implement");
    expect(state.canRunWithout).toBe(true);
    expect(state.options.map((o) => o.label)).toEqual([
      "Start and save",
      "Run without a loop",
      "Cancel",
    ]);
    // A review loop's card: two answers, nothing assumes three either.
    const review = setupCardState(
      card({
        summary:
          "api: review loop setup (first run: choose its models). Start and save these settings?\nProfile      high",
        options: [SETUP_START, SETUP_CANCEL],
      }),
    );
    expect(review.kind).toBe("review");
    expect(review.canRunWithout).toBe(false);
    expect(review.options.map((o) => o.answer)).toEqual([
      SETUP_START,
      SETUP_CANCEL,
    ]);
  });

  it("holds Start while something has to change first, never Run without a loop", () => {
    const options = setupOptions(
      [SETUP_START, SETUP_RUN_WITHOUT, SETUP_CANCEL],
      ["codex is not logged in"],
    );
    expect(options.map((o) => o.disabled)).toEqual([true, false, false]);
    // An answer the apps don't know shows as the hub wrote it.
    expect(setupOptions(["start over"])[0].label).toBe("Start over");
  });

  it("is one line in the tray and Needs you's summary, not the held task", () => {
    const result = attentionSummary({
      current: "default",
      hubId: "local",
      live: { threads: [worker()], attention: [card()] },
      liveStream: "live:1",
      sources: [],
    });
    expect(result.items[0].summary).toBe(
      "api: implementation loop setup (first run: choose its models). Start and save these settings?",
    );
    expect(result.items[0].card).toBe("loop_setup");
  });
});

describe("/implement", () => {
  const threads = [
    { id: "t1", name: "api", aliases: ["backend"] },
    { id: "t2", name: "web" },
  ];
  const live = (w: string) => threads.some((t) => t.name === w);

  it("splits the thread from the task as the core does", () => {
    expect(implementTarget(["api", "add", "a", "CSV"], false, live)).toEqual({
      thread: "api",
      task: "add a CSV",
    });
    // A first word that names no thread is the task's.
    expect(implementTarget(["add", "a", "CSV"], false, live)).toEqual({
      task: "add a CSV",
    });
    // In a thread's conversation every word is the task, a thread's name too.
    expect(implementTarget(["web", "pages", "render"], true, live)).toEqual({
      task: "web pages render",
    });
    expect(implementTarget(["api"], false, live)).toEqual({ thread: "api" });
    expect(implementTarget([], false, live)).toEqual({});
    expect(implementTarget(["  add ", "", "it"], false, live)).toEqual({
      task: "add it",
    });
  });

  it("asks for an implementation loop on the named, shown or selected thread", () => {
    const args = {
      words: ["backend", "add", "it"],
      setup: true,
      profile: "low",
      plan_check: "never",
      commit: null,
      reviewer: " codex/gpt-6.1-sol ",
    };
    expect(implementRequest(args, { threads })).toEqual({
      thread: "backend",
      kind: "implement",
      task: "add it",
      setup: true,
      overrides: {
        profile: "low",
        plan_check: "never",
        reviewer: "codex/gpt-6.1-sol",
      },
    });
    // In web's conversation, "api" is a word of the task.
    expect(
      implementRequest({ words: ["api", "docs"] }, { dive: "t2", threads }),
    ).toEqual({ thread: "t2", kind: "implement", task: "api docs" });
    // No words: the task is what the conversation settled.
    expect(implementRequest({ words: [] }, { chosen: "t1", threads })).toEqual({
      thread: "t1",
      kind: "implement",
    });
    expect(() =>
      implementRequest({ words: ["add", "it"] }, { threads }),
    ).toThrow(/No thread to run it in/);
  });
});

describe("Send as implementation loop", () => {
  it("takes the message as the task", () => {
    expect(sendAsLoop("add a CSV export\n\nwith tests  ")).toEqual({
      task: "add a CSV export\n\nwith tests",
      confirm: false,
    });
    // `//` sends a leading slash, as Send does.
    expect(sendAsLoop("//etc/hosts is wrong")).toEqual({
      task: "/etc/hosts is wrong",
      confirm: false,
    });
  });
  it("asks first when nothing was typed", () => {
    expect(sendAsLoop("")).toEqual({ confirm: true });
    expect(sendAsLoop("  \n ")).toEqual({ confirm: true });
  });
  it("takes no command, no shell line and no task over 16 KiB", () => {
    expect(() => sendAsLoop("/review")).toThrow(/A command is no task/);
    expect(() => sendAsLoop("!ls")).toThrow(/A shell command is no task/);
    expect(() => sendAsLoop("x".repeat(16 * 1024 + 1))).toThrow(/16 KiB/);
    expect(sendAsLoop("x".repeat(16 * 1024)).task).toHaveLength(16 * 1024);
  });
});

describe("the New thread form", () => {
  const command = commands.find((c) => c.id === "new")!;

  it("has a first prompt, then the switch, then the setup choice", () => {
    const keys = command.fields.map((f) => f.key);
    expect(keys.indexOf("prompt")).toBe(keys.indexOf("name") + 1);
    expect(keys.indexOf("implement")).toBe(keys.indexOf("prompt") + 1);
    expect(command.fields.find((f) => f.key === "implement")?.switch).toBe(
      true,
    );
  });

  it("sends thread.new with the loop, the first prompt being its task", () => {
    const params = parameters(command, {
      project: "api",
      prompt: "  add rate limiting  ",
      implement: true,
      loop_setup: true,
    });
    expect(newThreadRequest(params)).toEqual({
      params: {
        project: "api",
        loop: { kind: "implement", task: "add rate limiting", setup: true },
      },
    });
    expect(
      newThreadRequest(
        parameters(command, { project: "api", prompt: "go", implement: true }),
      ),
    ).toEqual({
      params: { project: "api", loop: { kind: "implement", task: "go" } },
    });
  });

  it("needs the task with the switch on", () => {
    expect(() =>
      newThreadRequest(
        parameters(command, { project: "api", prompt: " ", implement: true }),
      ),
    ).toThrow(/first prompt is the implementation loop's task/);
  });

  it("sends the first prompt once the thread exists, with the switch off", () => {
    expect(
      newThreadRequest(
        parameters(command, {
          project: "api",
          prompt: "look around",
          loop_setup: true,
        }),
      ),
    ).toEqual({ params: { project: "api" }, firstPrompt: "look around" });
    expect(newThreadRequest(parameters(command, { project: "api" }))).toEqual({
      params: { project: "api" },
    });
  });

  it("opens from /new … --implement with the switch on and the task in it", () => {
    expect(
      newThreadValues({
        project: "api",
        runtime: null,
        name: null,
        account: null,
        outside_pin: false,
        implement: true,
        setup: true,
        task: "add it",
      }),
    ).toEqual({
      project: "api",
      outside_pin: false,
      implement: true,
      prompt: "add it",
      loop_setup: true,
    });
    expect(
      newThreadValues({ project: "api", implement: false, task: null }),
    ).toEqual({ project: "api" });
  });

  it("reads what came of the loop thread.new asked for", () => {
    expect(newThreadLoop({ id: "t9" })).toBeUndefined();
    expect(
      newThreadLoop({ loop: { status: "needs_setup", attn_id: "a1" } }),
    ).toEqual({ status: "needs_setup", attn_id: "a1" });
    expect(newThreadLoop({ loop: { status: "started", loop: {} } })).toEqual({
      status: "started",
    });
    expect(
      newThreadLoop({
        loop: { status: "error", code: "conflict", message: "plan mode" },
      }),
    ).toEqual({ status: "error", message: "plan mode" });
  });
});

/** An implementation loop's state, as the core's fold gives it, at `phase`. */
const implementing = (extra: Partial<LoopState> = {}): LoopState => ({
  id: "l1",
  kind: "implement",
  profile: "high",
  phase: "plan",
  rounds: [],
  ledger: 0,
  verify: { done: 0, cap: 5 },
  paused: false,
  waiting_on: "thread",
  reviewer: { runtime: "codex", model: "gpt-6.1-sol" },
  plan: { sweep: true },
  commit: "end",
  ...extra,
});

describe("an implementation loop's phases", () => {
  it("marks the plan check skipped once past it without a round p1", () => {
    expect(skippedPhases(implementing({ phase: "plan_check" }))).toEqual([]);
    expect(
      skippedPhases(implementing({ phase: "implement", plan_revision: 1 })),
    ).toEqual(["plan_check"]);
    expect(
      skippedPhases(
        implementing({
          phase: "ladder",
          rounds: [{ id: "p1", findings: 2, new_class: 0 }],
        }),
      ),
    ).toEqual([]);
    // A review loop has no plan check to skip.
    expect(
      skippedPhases(implementing({ kind: "review", phase: "ladder" })),
    ).toEqual([]);
  });

  it("refuses Skip to verify before the ladder, with the reason", () => {
    for (const phase of ["plan", "plan_check", "implement"]) {
      const skip = skipToVerify({ phase });
      expect(skip).toMatchObject({ show: true, enabled: false });
      expect(skip.why).toMatch(/Nothing has been built to verify yet/);
    }
    expect(skipToVerify({ phase: "ladder" })).toEqual({
      show: true,
      enabled: true,
    });
    expect(skipToVerify({ phase: "ladder", skip: true }).show).toBe(false);
    expect(skipToVerify({ phase: "verify" }).show).toBe(false);
  });

  it("words the plan and the implement turn", () => {
    expect(
      planWords({
        revision: 2,
        contract_risk: true,
        reason: "a migration",
        path: "/l/plan.md",
      }),
    ).toBe("revision 2 · contract risk yes: a migration");
    expect(planWords({ revision: 1, contract_risk: false, reason: "" })).toBe(
      "revision 1 · contract risk no",
    );
    expect(
      implementedWords({
        status: "done",
        gates: { ran: ["cargo test"], result: "pass" },
        accepted: true,
      }),
    ).toBe("done · gates pass: cargo test");
    expect(
      implementedWords({
        status: "partial",
        gates: { ran: [], result: "fail" },
        accepted: false,
      }),
    ).toMatch(/^partial · gates fail \(sent back/);
  });

  it("counts no plan-check round among the rounds", () => {
    expect(
      countedRounds([{ id: "p1" }, { id: "m1" }, { id: "x1" }, { id: "v1" }]),
    ).toBe(3);
  });
});

describe("how a loop ended", () => {
  const ended = {
    id: "l1",
    kind: "implement",
    outcome: "clean",
    ended_turn: 9,
    commits: [{ repo: "/w/api", sha: "abc1234def" }],
  };

  it("names the commit, or why there is none", () => {
    expect(commitWords(ended)).toBe("committed abc1234");
    expect(
      commitWords({
        commits: [
          { repo: "/w/api", sha: "abc1234def" },
          { repo: "/w/web", sha: "0123456789" },
        ],
      }),
    ).toBe("committed abc1234 (api), 0123456 (web)");
    expect(commitWords({ commit_problem: "2 paths are uncommitted" })).toBe(
      "not committed: 2 paths are uncommitted",
    );
    expect(commitWords({})).toBe("");
  });

  it("lists what was committed before what was not, never `not committed` alone then", () => {
    const partly = {
      commits: [{ repo: "/w/api", sha: "abc1234def" }],
      commit_problem: "/w/web: 1 path is uncommitted beyond the baseline: a.rs",
    };
    expect(commitWords(partly)).toBe(
      "committed abc1234 · not committed: /w/web: 1 path is uncommitted beyond the baseline: a.rs",
    );
    expect(endedWords({ ...ended, ...partly }, 3)).toBe(
      "implementation loop clean after 3 rounds · committed abc1234 · not committed: /w/web: 1 path is uncommitted beyond the baseline: a.rs",
    );
    const first = endedLoops(undefined, [{ id: "t1", name: "api" }]);
    expect(
      endedLoops(first.known, [
        { id: "t1", name: "api", last_loop: { ...ended, ...partly } },
      ]).said,
    ).toEqual([
      "api's implementation loop ended: clean; committed abc1234 · not committed: /w/web: 1 path is uncommitted beyond the baseline: a.rs",
    ]);
  });

  it("reads the last commit check's notes, which are no problems", () => {
    expect(commitNotes(undefined)).toEqual([]);
    expect(commitNotes({ commit_checks: [] })).toEqual([]);
    expect(
      commitNotes({
        commit_checks: [
          { passed: false, problems: ["/w: dirty"], notes: ["old note"] },
          {
            passed: true,
            commits: [{ repo: "/w", sha: "abc1234" }],
            notes: [
              "the thread wrote no twr-commit block (the check reads git, not the block)",
              "",
            ],
          },
        ],
      }),
    ).toEqual([
      "the thread wrote no twr-commit block (the check reads git, not the block)",
    ]);
  });

  it("says it in a line, with the rounds when known", () => {
    expect(endedWords(ended, 3)).toBe(
      "implementation loop clean after 3 rounds · committed abc1234",
    );
    expect(
      endedWords(
        {
          id: "l2",
          kind: "review",
          outcome: "cap_expired",
        },
        1,
      ),
    ).toBe(
      "review loop verification cap reached with findings left after 1 round",
    );
    expect(
      endedWords({
        ...ended,
        commits: [],
        commit_problem: "1 path is uncommitted",
      }),
    ).toBe("implementation loop clean · not committed: 1 path is uncommitted");
  });

  it("is told to a screen reader once, with the commit", () => {
    const first = endedLoops(undefined, [{ id: "t1", name: "api" }]);
    const { said } = endedLoops(first.known, [
      { id: "t1", name: "api", last_loop: ended },
    ]);
    expect(said).toEqual([
      "api's implementation loop ended: clean; committed abc1234",
    ]);
    const notCommitted = endedLoops(first.known, [
      {
        id: "t1",
        name: "api",
        last_loop: { ...ended, commits: [], commit_problem: "dirty" },
      },
    ]);
    expect(notCommitted.said[0]).toBe(
      "api's implementation loop ended: clean; not committed: dirty",
    );
  });

  it("stays on the card while the loop's turn is the thread's last", () => {
    expect(
      endedLately(worker({ last_loop: ended, last_turn: { turn: 9 } })),
    ).toBe(ended);
    expect(
      endedLately(worker({ last_loop: ended, last_turn: { turn: 10 } })),
    ).toBeUndefined();
    expect(
      endedLately(
        worker({
          last_loop: ended,
          last_turn: { turn: 9 },
          loop: { id: "l2" },
        }),
      ),
    ).toBeUndefined();
  });

  it("says a started implementation loop as such", () => {
    expect(startedNotice("api", "implement", "high · plan")).toBe(
      "api started an implementation loop: high · plan.",
    );
    expect(startedNotice("api", "review")).toBe("api started a review loop.");
  });
});

describe("the Review loop offer while a setup card is open", () => {
  it("hides while the thread's own setup card is open, not another thread's", () => {
    const t = worker();
    expect(offersReview(t, true, [])).toBe(true);
    expect(offersReview(t, true, [{ thread: "t1", loop_setup: true }])).toBe(
      false,
    );
    expect(offersReview(t, true, [{ thread: "t2", loop_setup: true }])).toBe(
      true,
    );
    expect(offersReview(t, true, [{ thread: "t1" }])).toBe(true);
  });
});

describe("the notification of a loop's end", () => {
  const end = (id: string, extra: Partial<LoopEndSummary> = {}) => ({
    workspace: "default",
    thread: "t1",
    title: "api",
    id,
    words: "review loop clean after 6 rounds",
    ...extra,
  });
  const summary = (
    loops: LoopEndSummary[],
    stream = "live:1",
  ): AttentionSummary => ({
    count: 0,
    items: [],
    workspaces: [{ name: "default", stream }],
    loops,
  });

  it("puts each thread's last loop in the summary, with the rounds seen", () => {
    const result = attentionSummary({
      current: "default",
      hubId: "local",
      live: {
        threads: [
          worker({
            last_loop: {
              id: "l1",
              kind: "implement",
              outcome: "clean",
              commits: [{ repo: "/w", sha: "abc1234ff" }],
            },
          }),
          worker({ id: "t2", name: "web" }),
        ],
        attention: [],
      },
      liveStream: "live:1",
      sources: [],
      loopRounds: new Map([["l1", 4]]),
    });
    expect(result.loops).toEqual([
      {
        workspace: "default",
        thread: "t1",
        title: "api",
        id: "l1",
        words: "implementation loop clean after 4 rounds · committed abc1234",
      },
    ]);
    // The desktop's main process takes it as it is.
    expect(attentionSummarySchema.parse(result)).toEqual(result);
  });

  it("is news once, after the stream's first sight, and not while you look", () => {
    const watch = new LoopWatch();
    // What the stream lists first ended before.
    expect(watch.observe(summary([end("l0")]), false)).toEqual([]);
    expect(watch.observe(summary([end("l0")]), false)).toEqual([]);
    // A thread's next loop is news; the same one is not again.
    const next = end("l2");
    expect(watch.observe(summary([next]), false)).toEqual([next]);
    expect(watch.observe(summary([next]), false)).toEqual([]);
    // While the window is in front it is seen, not notified.
    expect(watch.observe(summary([end("l3")]), true)).toEqual([]);
    // A new stream (a reconnect) lists what ended before as known.
    expect(watch.observe(summary([end("l4")], "live:2"), false)).toEqual([]);
  });

  it("says how it ended, never what was asked", () => {
    const notices = loopNotices([
      end("l1", {
        words: "implementation loop clean after 3 rounds · committed abc1234",
      }),
    ]);
    expect(notices).toEqual([
      {
        title: "api · default",
        body: "Implementation loop clean after 3 rounds · committed abc1234",
        keys: ['["loop","default","t1","l1"]'],
        target: { workspace: "default", thread: "t1" },
      },
    ]);
    expect(isLoopNotice(notices[0].keys)).toBe(true);
    expect(isLoopNotice(['["default","a1",1]'])).toBe(false);
    const many = loopNotices(
      ["a", "b", "c", "d"].map((id) => end(id, { title: `t-${id}` })),
    );
    expect(many).toHaveLength(1);
    expect(many[0].title).toBe("4 loops ended");
  });

  describe("posted by the notifier", () => {
    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());
    const setup = (enabled = true) => {
      const state = { attended: false, enabled };
      const posted: { title: string; body: string; closed: boolean }[] = [];
      const post: Post = (notice) => {
        const record = { ...notice, closed: false };
        posted.push(record);
        return { close: () => (record.closed = true) };
      };
      const notifier = new Notifier(
        {
          enabled: () => state.enabled,
          attended: () => state.attended,
          open: () => {},
          log: { info: vi.fn(), warn: vi.fn() } as any,
        },
        { post, supported: true, now: () => Date.now() },
      );
      notifier.update(summary([end("l0")]));
      return { notifier, posted, state };
    };

    it("posts a loop's end once, at once, and keeps it when nothing waits", () => {
      const { notifier, posted } = setup();
      notifier.update(summary([end("l1")]));
      expect(posted).toEqual([
        {
          title: "api · default",
          body: "Review loop clean after 6 rounds",
          keys: ['["loop","default","t1","l1"]'],
          target: { workspace: "default", thread: "t1" },
          closed: false,
        },
      ]);
      // No request waits, yet the notice is no answered request: it stays.
      notifier.update(summary([end("l1")]));
      expect(posted).toHaveLength(1);
      expect(posted[0].closed).toBe(false);
      // The window comes to the front: it goes.
      notifier.attend();
      expect(posted[0].closed).toBe(true);
    });

    it("posts nothing with notifications off, or while you look", () => {
      const off = setup(false);
      off.notifier.update(summary([end("l1")]));
      expect(off.posted).toEqual([]);
      const looking = setup();
      looking.state.attended = true;
      looking.notifier.update(summary([end("l1")]));
      expect(looking.posted).toEqual([]);
    });
  });
});

describe("rendered", () => {
  (globalThis as any).window ??= { twr: {} };

  it("draws the implementation loop's card: its task in a scrolling box, three answers", async () => {
    const { LoopSetupCard } = await import("../shared/LoopViews");
    const html = renderToStaticMarkup(
      createElement(LoopSetupCard, {
        item: card(),
        thread: worker(),
        readOnly: false,
        onRespond: async () => {},
      }),
    );
    expect(html).toContain("Implementation loop setup");
    expect(html).toContain(
      '<div class="loop-setup-task" tabindex="0" role="region" aria-label="Task">add a CSV export to the reports page\nkeep the  columns as they are',
    );
    expect(html).toContain("(+2 more lines)");
    expect(html).toContain(
      "<dt>Plan check</dt><dd>contracts (the profile&#x27;s)</dd>",
    );
    expect(html).toContain("<dt>Commit</dt><dd>end</dd>");
    // Every answer waits for the 600 ms rule; keyboard order is Start, Change…, Run
    // without a loop, Cancel.
    const buttons = [...html.matchAll(/<button[^>]*>([^<]*)<\/button>/g)].map(
      (m) => [m[1], /aria-disabled="true"/.test(m[0])],
    );
    expect(buttons).toEqual([
      ["Start and save", true],
      ["Change…", false],
      ["Run without a loop", true],
      ["Cancel", true],
    ]);
  });

  it("has no Workers row for an implementer whose runtime runs no subagents", async () => {
    const { LoopSetupCard } = await import("../shared/LoopViews");
    // The hub leaves the row out of a card whose setup says `no_workers`; the card draws
    // the rows the hub wrote, and no other.
    const html = (summary: string) =>
      renderToStaticMarkup(
        createElement(LoopSetupCard, {
          item: card({ summary }),
          thread: worker(),
          readOnly: false,
          onRespond: async () => {},
        }),
      );
    expect(html(implementCard)).toContain(
      "<dt>Workers</dt><dd>claude-opus-5-5 · high</dd>",
    );
    const noWorkers = html(
      implementCard
        .split("\n")
        .filter((line) => !line.startsWith("Workers"))
        .join("\n"),
    );
    expect(noWorkers).not.toContain("Workers");
    expect(noWorkers).toContain("<dt>Implementer</dt>");
    expect(noWorkers).toContain("<dt>Reviewer</dt>");
  });

  it("is a row titled for its kind that opens a sheet on a phone", async () => {
    const { LoopSetupCard } = await import("../shared/LoopViews");
    const g = globalThis as any;
    const saved = { document: g.document, matchMedia: g.window.matchMedia };
    g.document = { documentElement: { dataset: { mobile: "true" } } };
    g.window.matchMedia = () => ({ matches: true });
    try {
      const html = renderToStaticMarkup(
        createElement(LoopSetupCard, {
          item: card(),
          thread: worker(),
          readOnly: false,
          onRespond: async () => {},
        }),
      );
      expect(html).toContain('aria-label="Implementation loop setup for api"');
      expect(html).toContain(">Review setup</button>");
      expect(html).not.toContain(">Run without a loop</button>");
    } finally {
      g.document = saved.document;
      g.window.matchMedia = saved.matchMedia;
    }
  });

  const section = async (thread: Thread) => {
    const { LoopsSection } = await import("../shared/LoopViews");
    return renderToStaticMarkup(
      createElement(LoopsSection, { connected: false, thread }),
    );
  };

  it("offers both loops, the implementation one asking for its task", async () => {
    const html = await section(worker());
    expect(html).toMatch(/Review loop<\/button>/);
    expect(html).toMatch(
      /<button type="button" disabled=""[^>]*aria-haspopup="dialog"[^>]*>.*Implementation loop…<\/button>/,
    );
    expect(html).toContain("Change…");
  });

  it("shows a running implementation loop's phases, plan and implement result", async () => {
    const html = await section(
      worker({
        loop: implementing({
          phase: "ladder",
          round: "m1",
          plan_revision: 1,
          contract_risk: false,
          rounds: [{ id: "m1", findings: 2, new_class: 1 }],
          waiting_on: "thread",
        }),
        loop_badge: "IMPLEMENT LOOP",
      }),
    );
    expect(html).toContain("IMPLEMENT LOOP");
    const phases = [...html.matchAll(/<li[^>]*>(.*?)<\/li>/g)]
      .map((m) => m[1].replace(/<[^>]+>/g, ""))
      .slice(0, 7);
    expect(phases).toEqual([
      "plan (done)",
      "plan check skipped",
      "implement (done)",
      "ladder",
      "sweep",
      "verify",
      "report",
    ]);
    expect(html).toContain('aria-current="step"');
    expect(html).toContain("<dt>Plan</dt><dd>revision 1 · contract risk no");
    expect(html).toContain("<dt>Commit</dt><dd>one commit at the end</dd>");
    expect(html).toContain("<b>m1</b> 2 findings · 1 new class");
    // In the ladder Skip to verify can be asked for (not here: no live connection).
    expect(html).toMatch(/>Skip to verify<\/button>/);
    expect(html).not.toContain("Nothing has been built to verify yet");
  });

  it("lists the plan check's round and holds Skip to verify before the ladder", async () => {
    const html = await section(
      worker({
        loop: implementing({
          phase: "plan_check",
          round: "p1",
          rounds: [{ id: "p1", findings: 0, new_class: 0 }],
          waiting_on: "review",
        }),
      }),
    );
    expect(html).toContain("<b>p1</b> under review");
    expect(html).toContain(
      "Nothing has been built to verify yet: Skip to verify is available from the ladder on.",
    );
    expect(html).toMatch(/aria-describedby="[^"]*-skip-why"/);
  });

  it("shows the commits at the end, or why there are none", async () => {
    const done = await section(
      worker({
        last_loop: {
          id: "l1",
          kind: "implement",
          outcome: "clean",
          ended_turn: 4,
          commits: [{ repo: "/w/api", sha: "abc1234def" }],
        },
      }),
    );
    expect(done).toContain("Last implementation loop: clean");
    expect(done).toContain("Committed <code>abc1234</code>");
    const failed = await section(
      worker({
        last_loop: {
          id: "l1",
          kind: "implement",
          outcome: "cap_expired",
          commit_problem: "1 path is uncommitted",
        },
      }),
    );
    expect(failed).toContain("<b>Not committed:</b> 1 path is uncommitted");
    expect(failed).not.toContain("Committed <code>");
    // Some repositories committed, another not: both, the commits first.
    const partly = await section(
      worker({
        last_loop: {
          id: "l1",
          kind: "implement",
          outcome: "clean",
          commits: [{ repo: "/w/api", sha: "abc1234def" }],
          commit_problem: "/w/web: 1 path is uncommitted",
        },
      }),
    );
    expect(partly).toContain("Committed <code>abc1234</code>");
    expect(partly).toContain(
      "<b>Not committed:</b> /w/web: 1 path is uncommitted",
    );
    expect(partly.indexOf("Committed <code>")).toBeLessThan(
      partly.indexOf("Not committed:"),
    );
  });

  it("draws the ended line, an uncommitted end not by colour alone", async () => {
    const { LoopEndedLine } = await import("../shared/LoopViews");
    const line = (last_loop: object) =>
      renderToStaticMarkup(
        createElement(LoopEndedLine, {
          thread: worker({ last_loop, last_turn: { turn: 4 } }),
        }),
      );
    const committed = line({
      id: "l1",
      kind: "implement",
      outcome: "clean",
      ended_turn: 4,
      commits: [{ repo: "/w", sha: "abc1234def" }],
    });
    expect(committed).toContain("IMPLEMENT LOOP");
    expect(committed).toContain("clean · committed abc1234");
    expect(committed).toContain(
      '<span class="visually-hidden"> ended: </span>',
    );
    const not = line({
      id: "l1",
      kind: "implement",
      outcome: "clean",
      ended_turn: 4,
      commit_problem: "dirty",
    });
    expect(not).toContain("not committed: dirty");
    expect(not).toContain("not-committed");
    const partly = line({
      id: "l1",
      kind: "implement",
      outcome: "clean",
      ended_turn: 4,
      commits: [{ repo: "/w/api", sha: "abc1234def" }],
      commit_problem: "/w/web: dirty",
    });
    expect(partly).toContain(
      "clean · committed abc1234 · not committed: /w/web: dirty",
    );
  });

  it("draws Send's caret, and the phone's ↻ button, named", async () => {
    const { SendAsLoop } = await import("../shared/LoopViews");
    const desktop = renderToStaticMarkup(
      createElement(SendAsLoop, {
        thread: worker(),
        phone: false,
        busy: false,
        onSend: () => {},
      }),
    );
    expect(desktop).toContain('aria-label="More ways to send"');
    expect(desktop).toContain('aria-expanded="false"');
    const phone = renderToStaticMarkup(
      createElement(SendAsLoop, {
        thread: worker(),
        phone: true,
        busy: false,
        onSend: () => {},
      }),
    );
    expect(phone).toContain('aria-label="Send as implementation loop to api…"');
    expect(phone).toContain('class="send-loop-button"');
  });
});
