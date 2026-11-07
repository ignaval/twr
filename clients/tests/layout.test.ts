import { describe, expect, it } from "vitest";
import { defaultLayout, validateLayout } from "../shared/layoutPreferences";

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
});
