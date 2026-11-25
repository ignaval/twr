import { describe, expect, it } from "vitest";
import {
  defaultLayout,
  steppedLayout,
  textSizeShortcut,
  textSizeStepGate,
  validateLayout,
} from "../shared/layoutPreferences";

describe("layout preferences", () => {
  it("recovers malformed saved preferences and bounds pane/text sizes", () => {
    expect(validateLayout(null)).toEqual(defaultLayout);
    expect(
      validateLayout({
        sidebar: Infinity,
        inspector: 9999,
        textSize: 0,
        codeSize: NaN,
        density: "dense",
      }),
    ).toEqual({ ...defaultLayout, inspector: 600, textSize: 12 });
    expect(
      validateLayout({
        sidebar: 0,
        inspector: -1,
        textSize: 99,
        codeSize: 99,
        density: "compact",
      }),
    ).toEqual({
      sidebar: 200,
      inspector: 280,
      textSize: 22,
      codeSize: 24,
      density: "compact",
    });
  });

  it("reads Ctrl/⌘ with +, =, - and 0 as text size steps", () => {
    const key = (key: string, mods: Partial<KeyboardEvent> = {}) => ({
      key,
      ctrlKey: false,
      metaKey: false,
      altKey: false,
      ...mods,
    });
    const ctrl = { ctrlKey: true };
    expect(textSizeShortcut(key("+", ctrl), false)).toBe("larger");
    expect(textSizeShortcut(key("=", ctrl), false)).toBe("larger");
    expect(textSizeShortcut(key("-", ctrl), false)).toBe("smaller");
    expect(textSizeShortcut(key("0", ctrl), false)).toBe("reset");
    expect(textSizeShortcut(key("=", { metaKey: true }), true)).toBe("larger");
    // The other platform's modifier, Alt, no modifier, or another key: not a step.
    expect(
      textSizeShortcut(key("=", { metaKey: true }), false),
    ).toBeUndefined();
    expect(textSizeShortcut(key("-", ctrl), true)).toBeUndefined();
    expect(
      textSizeShortcut(key("=", { ctrlKey: true, altKey: true }), false),
    ).toBeUndefined();
    expect(textSizeShortcut(key("="), false)).toBeUndefined();
    expect(textSizeShortcut(key("k", ctrl), false)).toBeUndefined();
  });

  it("steps interface and code text a pixel at a time within bounds", () => {
    expect(steppedLayout(defaultLayout, "larger")).toEqual({
      textSize: 14,
      codeSize: 14,
    });
    expect(steppedLayout(defaultLayout, "smaller")).toEqual({
      textSize: 12,
      codeSize: 12,
    });
    // At an end a size stays put while the other keeps moving.
    const small = { ...defaultLayout, textSize: 12, codeSize: 12 };
    expect(steppedLayout(small, "smaller")).toEqual({
      textSize: 12,
      codeSize: 11,
    });
    const large = { ...defaultLayout, textSize: 22, codeSize: 20 };
    expect(steppedLayout(large, "larger")).toEqual({
      textSize: 22,
      codeSize: 21,
    });
    expect(steppedLayout(large, "reset")).toEqual({
      textSize: defaultLayout.textSize,
      codeSize: defaultLayout.codeSize,
    });
  });

  it("counts one press once when both the keydown and the menu see it", () => {
    let now = 1000;
    const gate = textSizeStepGate(() => now);
    expect(gate("larger", "keyboard")).toBe(true);
    now += 5;
    expect(gate("larger", "menu")).toBe(false);
    // A held key both see: each repeat counts once.
    now += 30;
    expect(gate("larger", "keyboard")).toBe(true);
    now += 1;
    expect(gate("larger", "menu")).toBe(false);
    // Repeats from one source alone all count.
    now += 30;
    expect(gate("larger", "keyboard")).toBe(true);
    now += 30;
    expect(gate("larger", "keyboard")).toBe(true);
    // The other source later, or a different step, is a new press.
    now += 1000;
    expect(gate("larger", "keyboard")).toBe(true);
    now += 5;
    expect(gate("smaller", "menu")).toBe(true);
  });
});
