import { afterEach, describe, expect, it, vi } from "vitest";
import { SaveBeforeClose } from "../desktop/save-before-close";
import { onSaveNow, saveNow } from "../shared/save-now";

/* The desktop window has its page save the drafts before it closes or the app quits
 * (desktop/save-before-close.ts, main.ts), and the page runs what registered to save
 * (shared/save-now.ts). */

const flush = () => new Promise((resolve) => setTimeout(resolve));

describe("saving before the window closes", () => {
  afterEach(() => vi.useRealTimers());

  it("holds the close until the page has saved, then closes once more", async () => {
    const asked: number[] = [];
    const gate = new SaveBeforeClose((id) => {
      asked.push(id);
      return true;
    });
    let closes = 0;
    const again = () => {
      closes++;
      expect(gate.close(again)).toBe(true);
    };
    expect(gate.close(again)).toBe(false);
    // A second close while the page saves asks nothing more.
    expect(gate.close(again)).toBe(false);
    expect(asked).toEqual([1]);
    await flush();
    expect(closes).toBe(0);
    gate.saved(1);
    await flush();
    expect(closes).toBe(1);
    // The close after that one asks again.
    expect(gate.close(again)).toBe(false);
    expect(asked).toEqual([1, 2]);
  });

  it("closes without waiting when there is no page, and after the timeout when it never answers", async () => {
    vi.useFakeTimers();
    let closes = 0;
    const gone = new SaveBeforeClose(() => false);
    expect(gone.close(() => closes++)).toBe(false);
    await vi.advanceTimersByTimeAsync(0);
    expect(closes).toBe(1);
    const silent = new SaveBeforeClose(() => true, 2000);
    expect(silent.close(() => closes++)).toBe(false);
    await vi.advanceTimersByTimeAsync(1999);
    expect(closes).toBe(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(closes).toBe(2);
  });

  it("lets the close an update's restart makes go ahead, once it has saved", async () => {
    const gate = new SaveBeforeClose((id) => {
      queueMicrotask(() => gate.saved(id));
      return true;
    });
    await gate.beforeClosing();
    expect(gate.close(() => {})).toBe(true);
    // A restart that did not happen: the next close asks again.
    await gate.beforeClosing();
    gate.reset();
    expect(gate.close(() => {})).toBe(false);
  });
});

describe("the page saving now", () => {
  it("runs every saver, and resolves once all are done, a failed one too", async () => {
    const done: string[] = [];
    const off = [
      onSaveNow(async () => {
        await flush();
        done.push("drafts");
      }),
      onSaveNow(async () => {
        throw new Error("storage full");
      }),
    ];
    await saveNow();
    expect(done).toEqual(["drafts"]);
    off.forEach((remove) => remove());
    await saveNow();
    expect(done).toEqual(["drafts"]);
  });
});
