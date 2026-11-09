import { describe, expect, it } from "vitest";
import { DEFAULT_WINDOW, restoreWindow } from "../desktop/window-state";

const laptop = { x: 0, y: 25, width: 1512, height: 920 };
const monitor = { x: 1512, y: 0, width: 2560, height: 1415 };
const state = (bounds: object, extra: object = {}) => ({
  maximized: false,
  fullscreen: false,
  ...bounds,
  ...extra,
});

describe("window placement", () => {
  it("opens at the default size without a saved state or with a corrupt one", () => {
    const fallback = {
      bounds: DEFAULT_WINDOW,
      maximized: false,
      fullscreen: false,
    };
    expect(restoreWindow(undefined, [laptop])).toEqual(fallback);
    expect(restoreWindow({ x: "1" }, [laptop])).toEqual(fallback);
    expect(
      restoreWindow(
        state({ x: 0, y: 0, width: 800, height: 600 }, { extra: 1 }),
        [laptop],
      ),
    ).toEqual(fallback);
  });

  it("restores a window that fits where it was, with its state", () => {
    expect(
      restoreWindow(
        state(
          { x: 1700, y: 100, width: 1400, height: 900 },
          { maximized: true },
        ),
        [laptop, monitor],
      ),
    ).toEqual({
      bounds: { x: 1700, y: 100, width: 1400, height: 900 },
      maximized: true,
      fullscreen: false,
    });
  });

  it("moves and shrinks a window onto the display that shows most of it", () => {
    // Mostly on the laptop, hanging off its bottom-right edge, taller than it.
    expect(
      restoreWindow(state({ x: 900, y: 300, width: 800, height: 1200 }), [
        laptop,
        monitor,
      ]).bounds,
    ).toEqual({ x: 712, y: 25, width: 800, height: 920 });
  });

  it("uses the default placement when its display is gone", () => {
    // Saved on the external monitor, which is no longer connected.
    expect(
      restoreWindow(state({ x: 2000, y: 200, width: 2000, height: 1200 }), [
        laptop,
      ]),
    ).toEqual({
      bounds: { width: 1512, height: 920 },
      maximized: false,
      fullscreen: false,
    });
    // A sliver of the window on screen is not enough to grab it.
    expect(
      restoreWindow(state({ x: 1480, y: 100, width: 800, height: 600 }), [
        laptop,
      ]).bounds,
    ).toEqual({ width: 800, height: 600 });
  });

  it("never restores below the minimum size", () => {
    expect(
      restoreWindow(state({ x: 10, y: 40, width: 100, height: 100 }), [laptop])
        .bounds,
    ).toEqual({ x: 10, y: 40, width: 420, height: 480 });
  });
});
