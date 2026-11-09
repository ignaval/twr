import { describe, expect, it } from "vitest";
import { messageTime } from "../shared/transcript";

describe("message times", () => {
  const now = new Date(2026, 10, 9, 18, 30);
  it("shows the time alone for today's messages", () => {
    const today = new Date(2026, 10, 9, 9, 5).toISOString();
    expect(messageTime(today, now)).toBe(
      new Date(today).toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
      }),
    );
  });
  it("adds the day for older messages, and the year for another year's", () => {
    const lastWeek = messageTime(
      new Date(2026, 10, 2, 9, 5).toISOString(),
      now,
    );
    const lastYear = messageTime(
      new Date(2025, 10, 2, 9, 5).toISOString(),
      now,
    );
    expect(lastWeek).not.toBe(lastYear);
    expect(lastWeek).not.toContain("2026");
    expect(lastYear).toContain("2025");
    expect(lastWeek.length).toBeGreaterThan(
      messageTime(new Date(2026, 10, 9, 9, 5).toISOString(), now).length,
    );
  });
  it("shows nothing for an unreadable time", () => {
    expect(messageTime("not a time", now)).toBe("");
  });
});
