import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  accountsOf,
  ageText,
  levelOf,
  paceLeftPct,
  remainingPct,
  resetText,
  waitText,
  windowName,
  windowView,
} from "../shared/usage-limits";
import { UsageLimits } from "../shared/UsageLimits";

const now = Date.parse("2026-11-20T12:00:00Z");
const at = (minutes: number) => new Date(now + minutes * 60_000).toISOString();

describe("usage limits arithmetic", () => {
  it("shows the share left, clamped, from remaining_pct or used_pct", () => {
    expect(remainingPct({ remaining_pct: 77.5, used_pct: 22.5 })).toBe(77.5);
    expect(remainingPct({ used_pct: 30 })).toBe(70);
    expect(remainingPct({ used_pct: 104.4 })).toBe(0);
    expect(remainingPct({ used_pct: -3 })).toBe(100);
    expect(remainingPct({ used_pct: "x" })).toBe(0);
  });

  it("warns at 25% left or less and is low at 10% or less, before rounding", () => {
    expect(levelOf(25.4)).toBe("normal");
    expect(levelOf(25)).toBe("warn");
    expect(levelOf(10.2)).toBe("warn");
    expect(levelOf(10)).toBe("low");
    expect(levelOf(0)).toBe("low");
  });

  it("puts even pace where the window's time left is, only when its length and reset are known", () => {
    // A 5-hour window resetting in 2 hours: 40% of its time is left.
    expect(paceLeftPct({ minutes: 300, resets_at: at(120) }, now)).toBe(40);
    expect(paceLeftPct({ minutes: 300, resets_at: at(400) }, now)).toBe(100);
    expect(paceLeftPct({ minutes: 300, resets_at: at(-5) }, now)).toBe(0);
    expect(paceLeftPct({ resets_at: at(120) }, now)).toBeUndefined();
    expect(paceLeftPct({ minutes: 300 }, now)).toBeUndefined();
    expect(
      paceLeftPct({ minutes: null, resets_at: at(1) }, now),
    ).toBeUndefined();
  });

  it("says resets and ages in short relative units", () => {
    expect(waitText(30_000)).toBe("under a minute");
    expect(waitText(45 * 60_000)).toBe("45m");
    expect(waitText(120 * 60_000)).toBe("2h");
    expect(waitText(125 * 60_000)).toBe("2h 5m");
    expect(waitText((3 * 24 + 4) * 3_600_000)).toBe("3d 4h");
    expect(resetText(at(120), now)).toBe("resets in 2h");
    expect(resetText(at(-1), now)).toBe("resets now");
    expect(ageText(40)).toBe("40s");
    expect(ageText(12 * 60 + 5)).toBe("12m");
    expect(ageText(3 * 3600)).toBe("3h");
  });

  it("names the windows", () => {
    expect(windowName({ window: "five_hour", label: "5h" })).toBe("5-hour");
    expect(windowName({ window: "seven_day", label: "wk" })).toBe("Weekly");
    expect(windowName({ window: "month", label: "mo" })).toBe("Monthly");
    expect(windowName({ window: "90m", label: "90m" })).toBe("90m");
  });

  it("writes the tooltip as T3 Code does, with the reset's local time", () => {
    const view = windowView(
      { window: "five_hour", used_pct: 58, minutes: 300, resets_at: at(120) },
      now,
    );
    expect(view.leftText).toBe("42% left");
    expect(view.title).toMatch(
      /^42% left · 40% of the window left\. The line is where even spending would be\. Resets .+\.$/,
    );
    expect(view.valueText).toBe(
      "42% left, 40% of the window's time left, resets in 2h",
    );
    const plain = windowView({ window: "x", used_pct: 10 }, now);
    expect(plain.title).toBe("90% left.");
    expect(plain.pace).toBeUndefined();
    expect(plain.reset).toBeUndefined();
  });

  it("ages the data from when it was read, and drops windows that have reset", () => {
    const data = {
      runtimes: [
        {
          runtime: "claude-code",
          account: "work",
          label: "Claude:work",
          plan: "max",
          age_secs: 300,
          stale: false,
          windows: [
            { window: "five_hour", used_pct: 20, resets_at: at(-1) },
            { window: "seven_day", used_pct: 40, resets_at: at(600) },
          ],
        },
        {
          runtime: "codex",
          label: "GPT",
          age_secs: 0,
          windows: [{ window: "five_hour", used_pct: 1, resets_at: at(-2) }],
        },
      ],
    };
    const [claude, ...rest] = accountsOf(data, now, now - 360_000);
    expect(rest).toEqual([]);
    expect(claude.label).toBe("Claude:work");
    expect(claude.plan).toBe("max");
    expect(claude.windows.map((w) => w.name)).toEqual(["Weekly"]);
    expect(claude.age).toBe(660);
    expect(claude.stale).toBe(true);
    expect(accountsOf(data, now, now)[0].stale).toBe(false);
  });
});

describe("usage limits rendering", () => {
  const render = (runtimes: unknown[] | undefined, error?: string) =>
    renderToStaticMarkup(
      createElement(UsageLimits, {
        accounts:
          runtimes === undefined ? undefined : accountsOf({ runtimes }, now),
        error,
      }),
    );

  it("draws each window's share left, its level, the pace tick and the reset", () => {
    const html = render([
      {
        runtime: "claude-code",
        label: "Claude",
        plan: "pro",
        age_secs: 30,
        windows: [
          {
            window: "five_hour",
            used_pct: 92,
            minutes: 300,
            resets_at: at(90),
          },
          { window: "seven_day", used_pct: 80, resets_at: at(3000) },
          { window: "month", used_pct: 10 },
        ],
      },
    ]);
    expect(html).toContain("<b>Claude</b>");
    expect(html).toContain(" · pro");
    expect(html).toContain('data-level="low"');
    expect(html).toContain('data-level="warn"');
    expect(html).toContain('data-level="normal"');
    expect(html).toContain("8% left");
    expect(html).toContain('style="width:8%"');
    expect(html).toContain('role="meter"');
    expect(html).toContain('aria-label="Claude 5-hour"');
    expect(html).toContain('aria-valuenow="8"');
    // The pace tick only where the window says its length: 90 of 300 minutes left.
    expect(html.match(/class="usage-pace"/g)).toHaveLength(1);
    expect(html).toContain('style="left:30%"');
    expect(html).toContain("resets in 1h 30m");
    expect(html).not.toContain("as of");
    expect(html).not.toContain("% used");
  });

  it("dims stale data and says how old it is", () => {
    const html = render([
      {
        runtime: "codex",
        label: "GPT",
        age_secs: 720,
        stale: true,
        windows: [{ window: "seven_day", used_pct: 50 }],
      },
    ]);
    expect(html).toContain("data-stale");
    expect(html).toContain("as of 12m ago");
  });

  it("says when no session has reported usage, and when it is still reading", () => {
    expect(render([])).toContain("No session has reported usage yet");
    expect(render(undefined)).toContain("Reading usage");
    expect(render(undefined, "Not connected")).toContain("Not connected");
  });
});
