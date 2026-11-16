import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
const native = vi.hoisted(() => ({
  setStyle: vi.fn(async () => {}),
  setAppearance: vi.fn(async () => {}),
}));
vi.mock("@capacitor/core", () => ({
  Capacitor: { isNativePlatform: () => true },
  SystemBars: { setStyle: native.setStyle },
  SystemBarsStyle: { Dark: "DARK", Light: "LIGHT", Default: "DEFAULT" },
}));
vi.mock("../mobile/src/layout", () => ({
  Layout: { setAppearance: native.setAppearance },
}));
import { observeNativeAppearance } from "../mobile/src/appearance";

/** What the page's computed style says; a test changes it as the stylesheet "applies". */
let page: { bg: string; scheme: string };
let listeners: Map<string, () => void>;
const flush = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
};
beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  // A bare page on a light OS: no theme variables yet, the transparent background.
  page = { bg: "", scheme: "normal" };
  listeners = new Map();
  vi.stubGlobal("document", {
    documentElement: {},
    // The transparent pixel a bare page's background paints.
    createElement: () => ({
      getContext: () => ({
        clearRect() {},
        fillRect() {},
        getImageData: () => ({ data: [0, 0, 0, 0] }),
      }),
    }),
  });
  vi.stubGlobal("getComputedStyle", () => ({
    colorScheme: page.scheme,
    backgroundColor: "rgba(0, 0, 0, 0)",
    getPropertyValue: (name: string) => (name === "--bg" ? page.bg : ""),
  }));
  vi.stubGlobal("matchMedia", () => ({
    matches: false,
    addEventListener() {},
    removeEventListener() {},
  }));
  vi.stubGlobal(
    "MutationObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.stubGlobal("requestAnimationFrame", (run: () => void) =>
    setTimeout(run, 16),
  );
  vi.stubGlobal("cancelAnimationFrame", (id: number) => clearTimeout(id));
  vi.stubGlobal("addEventListener", (name: string, run: () => void) =>
    listeners.set(name, run),
  );
  vi.stubGlobal("removeEventListener", (name: string) =>
    listeners.delete(name),
  );
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("native appearance at startup", () => {
  it("applies the theme at once when the stylesheet is already there", async () => {
    page = { bg: "#0e1013", scheme: "dark" };
    const stop = observeNativeAppearance();
    await flush();
    expect(native.setStyle).toHaveBeenCalledWith({ style: "DARK" });
    expect(native.setAppearance).toHaveBeenCalledWith({
      background: "#0e1013",
    });
    stop();
  });
  it("waits for the stylesheet instead of sending a bare page's colors", async () => {
    const stop = observeNativeAppearance();
    await flush();
    await vi.advanceTimersByTimeAsync(400);
    // Not the light bars and black background a bare page computes to.
    expect(native.setStyle).not.toHaveBeenCalled();
    expect(native.setAppearance).not.toHaveBeenCalled();
    page = { bg: "#0e1013", scheme: "dark" };
    await vi.advanceTimersByTimeAsync(60);
    await flush();
    expect(native.setStyle).toHaveBeenCalledTimes(1);
    expect(native.setStyle).toHaveBeenCalledWith({ style: "DARK" });
    expect(native.setAppearance).toHaveBeenCalledWith({
      background: "#0e1013",
    });
    stop();
  });
  it("syncs again when the page has loaded", async () => {
    const stop = observeNativeAppearance();
    await flush();
    page = { bg: "#fbfbfa", scheme: "light" };
    listeners.get("load")!();
    await vi.advanceTimersByTimeAsync(20);
    await flush();
    expect(native.setStyle).toHaveBeenCalledWith({ style: "LIGHT" });
    expect(native.setAppearance).toHaveBeenCalledWith({
      background: "#fbfbfa",
    });
    stop();
    expect(listeners.has("load")).toBe(false);
  });
  it("uses what is there when no stylesheet ever defines the background", async () => {
    const stop = observeNativeAppearance();
    await vi.advanceTimersByTimeAsync(5200);
    await flush();
    expect(native.setAppearance).toHaveBeenCalledWith({
      background: "#000000",
    });
    stop();
  });
  it("stops looking once it is disposed", async () => {
    const stop = observeNativeAppearance();
    stop();
    page = { bg: "#0e1013", scheme: "dark" };
    await vi.advanceTimersByTimeAsync(6000);
    await flush();
    expect(native.setStyle).not.toHaveBeenCalled();
  });
});
