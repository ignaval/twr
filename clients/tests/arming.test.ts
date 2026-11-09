import { describe, expect, it } from "vitest";
import {
  ARM_MS,
  armed,
  armsIn,
  hidden,
  mayAnswer,
  shown,
} from "../shared/arming";

describe("attention answer arming", () => {
  it("answers nothing until the options have been on screen for 600 ms", () => {
    expect(ARM_MS).toBe(600);
    const state = shown(hidden(), 1000);
    expect(armed(state, 1000)).toBe(false);
    expect(armed(state, 1599)).toBe(false);
    expect(armsIn(state, 1200)).toBe(400);
    expect(armed(state, 1600)).toBe(true);
    expect(armsIn(state, 2000)).toBe(0);
  });

  it("is never armed while the options are off screen", () => {
    expect(armed(hidden(), 1e9)).toBe(false);
    expect(armsIn(hidden(), 1e9)).toBeUndefined();
  });

  it("keeps the first time they were seen while they stay on screen", () => {
    const first = shown(hidden(), 1000);
    // A later visibility report (another observer callback) doesn't restart it.
    expect(shown(first, 1500)).toBe(first);
    expect(armed(shown(first, 1500), 1600)).toBe(true);
  });

  it("starts again from zero after the options leave the screen", () => {
    const seen = shown(hidden(), 1000);
    expect(armed(seen, 5000)).toBe(true);
    const back = shown(hidden(), 5000);
    expect(armed(back, 5000)).toBe(false);
    expect(armed(back, 5600)).toBe(true);
  });

  it("refuses answers while busy, after one was sent and on read-only devices", () => {
    const arming = shown(hidden(), 0);
    const base = {
      arming,
      now: 1000,
      busy: false,
      sent: false,
      readOnly: false,
    };
    expect(mayAnswer(base)).toBe(true);
    expect(mayAnswer({ ...base, now: 100 })).toBe(false);
    expect(mayAnswer({ ...base, busy: true })).toBe(false);
    expect(mayAnswer({ ...base, sent: true })).toBe(false);
    expect(mayAnswer({ ...base, readOnly: true })).toBe(false);
    expect(mayAnswer({ ...base, arming: hidden() })).toBe(false);
  });

  it("the double click that answered one card can't answer the card sliding into place", () => {
    // Item A is answered by the first click at t=10000; B, mounted when A's card left at
    // t=10080, sits under the pointer when the second click lands at t=10180.
    const b = shown(hidden(), 10080);
    expect(
      mayAnswer({
        arming: b,
        now: 10180,
        busy: false,
        sent: false,
        readOnly: false,
      }),
    ).toBe(false);
  });
});
