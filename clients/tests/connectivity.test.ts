import { describe, expect, it } from "vitest";
import {
  attentionKindLabel,
  clockTime,
  hubOffline,
  isOffline,
  offlineNoticeText,
  offlineThreads,
} from "../shared/connectivity";
import { attentionSummary, cardOf } from "../shared/attentionSummary";
import { attentionSummarySchema } from "../desktop/validation";
import { kind } from "../desktop/notify-state";
import { attentionAnnouncement } from "../shared/a11y";
import { announcements } from "../shared/voice";
import type { Attention, Thread } from "../shared/types";

const since = new Date(2026, 10, 23, 14, 2).toISOString();
const now = new Date(2026, 10, 23, 14, 30);

/** The hub's notice (twr-hub `hub::net`), as `attention.list` gives it. */
const notice = {
  attn_id: "n1",
  revision: 4,
  thread: "hub",
  reason: "stalled",
  summary: "No internet connection since 14:02: twr cannot reach …",
  options: ["dismiss"],
  offline_since: since,
} as Attention;
const stall = (thread: string, offline = true) =>
  ({
    attn_id: `${thread}:s1`,
    revision: 5,
    thread,
    reason: "stalled",
    summary: "No internet connection since 14:02: no activity for 10 min",
    options: ["dismiss"],
    ...(offline ? { offline_since: since } : {}),
  }) as Attention;

describe("the hub's internet connection", () => {
  it("finds the hub's notice and counts the threads waiting on it", () => {
    expect(hubOffline([])).toBeUndefined();
    // A thread's stall marked offline is not the hub's notice.
    expect(hubOffline([stall("api")])).toBeUndefined();
    expect(hubOffline([stall("api"), notice])).toBe(notice);
    expect(
      offlineThreads([notice, stall("api"), stall("web"), stall("x", false)]),
    ).toBe(2);
    expect(isOffline(stall("x", false))).toBe(false);
    expect(isOffline({ offline_since: "" })).toBe(false);
  });

  it("says since when, in this device's time", () => {
    expect(clockTime(since, now)).toBe("14:02");
    const yesterday = new Date(2026, 10, 22, 23, 50).toISOString();
    expect(clockTime(yesterday, now)).toMatch(/^\S+ 23:50$/);
    expect(clockTime("not a time", now)).toBeUndefined();
    expect(offlineNoticeText(notice, 0, now)).toBe(
      "No internet connection on the hub's machine since 14:02: agents there can't reach " +
        "their model APIs. twr holds stall alerts and automatic retries until it's back.",
    );
    expect(offlineNoticeText(notice, 1, now)).toMatch(
      / 1 thread is waiting on it\.$/,
    );
    expect(offlineNoticeText(notice, 3, now)).toMatch(
      / 3 threads are waiting on it\.$/,
    );
    expect(offlineNoticeText({}, 0, now)).toMatch(
      /^No internet connection on the hub's machine: /,
    );
  });

  it("labels its cards as such, not as a stall", () => {
    expect(attentionKindLabel(notice)).toBe("No internet");
    expect(attentionKindLabel(stall("api"))).toBe("No internet");
    expect(attentionKindLabel(stall("api", false))).toBe("stalled");
    expect(attentionKindLabel({ reason: "permission" })).toBe(
      "Permission requested",
    );
    expect(attentionKindLabel({ reason: "question" })).toBe("Question for you");
    expect(attentionKindLabel({ reason: "error", loop_stuck: true })).toBe(
      "Loop stuck",
    );
    expect(
      attentionAnnouncement({ reason: "stalled", offline: true, summary: "x" }),
    ).toBe("Needs you: twr has no internet connection: x");
  });

  it("tells the desktop's tray and notifications what it is", () => {
    expect(cardOf(notice)).toBe("offline");
    expect(cardOf(stall("api"))).toBe("offline");
    expect(cardOf(stall("api", false))).toBeUndefined();
    const result = attentionSummary({
      current: "default",
      hubId: "local",
      live: {
        threads: [{ id: "api", name: "api", status: "running" } as Thread],
        attention: [notice, stall("api")],
      },
      liveStream: "live:1",
      sources: [],
    });
    expect(result.items.map((i) => i.card)).toEqual(["offline", "offline"]);
    expect(attentionSummarySchema.safeParse(result).success).toBe(true);
    expect(kind(result.items[0])).toBe("Has no internet connection");
  });

  it("knows a speaker's announcement of it when it is heard back", () => {
    expect(announcements([{ name: "api" }])).toContain(
      "twr has no internet connection",
    );
    expect(announcements([{ name: "api" }])).toContain(
      "api has no internet connection",
    );
  });
});
