import { describe, expect, it } from "vitest";
import {
  boxAway,
  boxWaits,
  loopAccess,
  offersReview,
  roundWords,
  startAccess,
  waitingWords,
  type LoopState,
} from "../shared/loops";
import type { Thread } from "../shared/types";

// A loop on a thread on a remote box (docs/design/loops.md, "Remote boxes"): when the box goes
// away while the loop needs it, the hub records `loop.box` and the loop waits; the apps say
// so in words, announce it once, and offer no start the hub would refuse.
const thread = (over: Partial<Thread> = {}): Thread =>
  ({
    id: "t1",
    name: "api",
    runtime: "fake",
    box_name: "devbox",
    box_online: true,
    mode: "auto",
    status: "done",
    accent: "",
    is_orchestrator: false,
    ...over,
  }) as Thread;

const waiting = (over: Partial<LoopState> = {}): LoopState => ({
  id: "L1",
  kind: "review",
  profile: "low",
  phase: "ladder",
  round: "m1",
  rounds: [{ id: "m1", findings: 0, new_class: 0, failed: true }],
  ledger: 0,
  verify: { done: 0, cap: 1 },
  paused: false,
  waiting_on: "review",
  box_away: "devbox",
  retried: true,
  reviewer: { runtime: "codex", model: "gpt-6.1-sol", effort: "medium" },
  review_started_at: null,
  ...over,
});

describe("a loop waiting on its box", () => {
  it("says so in the inspector, by the box's name", () => {
    expect(waitingWords(waiting())).toBe("waiting on box devbox");
    expect(waitingWords(waiting({ paused: true }))).toBe(
      "paused, waiting on box devbox",
    );
    // A stuck loop is stuck first; back, it waits on the review again.
    expect(waitingWords(waiting({ stuck: "boom" }))).toBe("stuck: boom");
    expect(waitingWords(waiting({ box_away: null }))).toBe(
      "waiting on the review",
    );
  });

  it("says the round's run failed and waits for the box, and that its retry is under review", () => {
    const l = waiting();
    expect(roundWords(l.rounds[0], l)).toBe("waits for box devbox");
    // The box back: the same round's retry runs. Its first run failed, this one is under
    // review (the hub keeps `failed` until the round answers).
    const back = waiting({
      box_away: null,
      review_started_at: "2026-11-14T00:00:00Z",
    });
    expect(roundWords(back.rounds[0], back)).toBe("under review again");
    const between = waiting({ box_away: null });
    expect(roundWords(between.rounds[0], between)).toBe("review run failed");
    // An earlier round that failed, or a stuck loop's: as before.
    const stuck = waiting({ box_away: null, waiting_on: "user" });
    expect(roundWords(stuck.rounds[0], stuck)).toBe("review run failed");
  });

  it("is announced once when it starts and once when it goes on, never when first seen", () => {
    const t = (loop?: Partial<LoopState>) => ({
      id: "t1",
      name: "api",
      loop: loop ? waiting(loop) : undefined,
    });
    // The app opened while the loop already waited: shown, not said.
    let r = boxWaits(undefined, [t({})]);
    expect(r.said).toEqual([]);
    r = boxWaits(r.known, [t({})]);
    expect(r.said).toEqual([]);
    r = boxWaits(r.known, [t({ box_away: null })]);
    expect(r.said).toEqual(["api's review loop goes on: box devbox is back"]);
    r = boxWaits(r.known, [t({ box_away: null })]);
    expect(r.said).toEqual([]);
    r = boxWaits(r.known, [t({ kind: "implement" })]);
    expect(r.said).toEqual([
      "api's implementation loop waits on box devbox: it goes on when the box is back",
    ]);
    // A loop that ends while it waits is said as ended, not as going on.
    r = boxWaits(r.known, [t()]);
    expect(r.said).toEqual([]);
  });
});

describe("starting a loop on a thread whose box is away", () => {
  it("is not offered: the hub refuses it", () => {
    const access = loopAccess({ connected: true });
    expect(boxAway(thread({ box_online: false }))).toBe("devbox");
    expect(boxAway(thread())).toBeUndefined();
    expect(
      boxAway(thread({ box_name: "local", box_online: false })),
    ).toBeUndefined();
    expect(startAccess(access, thread({ box_online: false }))).toEqual({
      act: false,
      why: "Box devbox is away: a loop can start once it is back.",
    });
    expect(startAccess(access, thread())).toEqual({ act: true });
    // A device that can't act anyway says why it can't.
    const readOnly = loopAccess({ scopes: ["read"], connected: true });
    expect(startAccess(readOnly, thread({ box_online: false }))).toBe(readOnly);
  });

  it("hides the Review loop offer while the box is away", () => {
    const changed = {
      last_turn: { turn: 2 },
      git: { dirty_files: 1 },
      current_turn: null,
    };
    expect(offersReview(thread(changed), true)).toBe(true);
    expect(offersReview(thread({ ...changed, box_online: false }), true)).toBe(
      false,
    );
  });
});
