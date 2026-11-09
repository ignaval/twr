import { describe, expect, it } from "vitest";
import { attentionKey } from "../desktop/tray-state";

const item = (thread: string, summary = "Needs approval") => ({
  thread,
  title: `Thread ${thread}`,
  summary,
});

describe("tray attention key", () => {
  it("is stable for a repeated summary and changes with anything the tray shows", () => {
    const base = { count: 2, items: [item("a"), item("b")] };
    const key = attentionKey(base);
    expect(attentionKey({ count: 2, items: [item("a"), item("b")] })).toBe(key);
    expect(attentionKey({ ...base, count: 3 })).not.toBe(key);
    expect(attentionKey({ count: 2, items: [item("b"), item("a")] })).not.toBe(
      key,
    );
    expect(
      attentionKey({ count: 2, items: [item("a"), item("b", "Asked")] }),
    ).not.toBe(key);
  });

  it("ignores items past the five the menu lists", () => {
    const five = Array.from({ length: 5 }, (_, index) => item(String(index)));
    expect(attentionKey({ count: 9, items: [...five, item("x")] })).toBe(
      attentionKey({ count: 9, items: [...five, item("y")] }),
    );
  });
});
