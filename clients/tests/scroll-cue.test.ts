import { describe, expect, it } from "vitest";
import { moreBelow, scrollCue } from "../shared/scrollCue";

/** A scrolling area's metrics and the parts of an element `scrollCue` uses. */
function area(scrollTop: number, clientHeight: number, scrollHeight: number) {
  const listeners = new Set<() => void>();
  return {
    scrollTop,
    clientHeight,
    scrollHeight,
    dataset: {} as Record<string, string>,
    children: [],
    addEventListener: (_: string, listener: () => void) =>
      listeners.add(listener),
    removeEventListener: (_: string, listener: () => void) =>
      listeners.delete(listener),
    listeners,
  };
}

describe("the cue that a request goes on below its area's fold", () => {
  it("shows while the area holds more below its bottom edge", () => {
    // A 236 px area (35% of a 674 px window) holding a 330 px setup card.
    expect(
      moreBelow({ scrollTop: 0, clientHeight: 236, scrollHeight: 330 }),
    ).toBe(true);
    expect(
      moreBelow({ scrollTop: 50, clientHeight: 236, scrollHeight: 330 }),
    ).toBe(true);
  });

  it("goes once the area is scrolled to its end, or holds all it has", () => {
    expect(
      moreBelow({ scrollTop: 94, clientHeight: 236, scrollHeight: 330 }),
    ).toBe(false);
    expect(
      moreBelow({ scrollTop: 0, clientHeight: 330, scrollHeight: 330 }),
    ).toBe(false);
    // A fractional scroll position that rounds to the end.
    expect(
      moreBelow({ scrollTop: 93.5, clientHeight: 236, scrollHeight: 330 }),
    ).toBe(false);
  });

  it("marks the area at once and stops listening when it goes", () => {
    const tall = area(0, 236, 330);
    const stop = scrollCue(tall as unknown as HTMLElement);
    expect(tall.dataset.more).toBe("below");
    expect(tall.listeners.size).toBe(1);
    stop?.();
    expect(tall.listeners.size).toBe(0);
    const short = area(0, 236, 120);
    scrollCue(short as unknown as HTMLElement)?.();
    expect(short.dataset.more).toBeUndefined();
    expect(scrollCue(null)).toBeUndefined();
  });
});
