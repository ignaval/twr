import { describe, it, expect } from "vitest";
import { usableViewportHeight } from "../mobile/src/geometry";
describe("folded keyboard layout", () => {
  const fold = { orientation: "horizontal" as const, top: 400, height: 20 };
  it("uses the upper pane when the keyboard leaves too little room below the hinge", () => {
    expect(usableViewportHeight(fold, 480)).toBe(400);
    expect(usableViewportHeight(fold, 350)).toBe(350);
  });
  it("restores both panes when there is usable room below the hinge", () => {
    expect(usableViewportHeight(fold, 800)).toBe(800);
    expect(usableViewportHeight(fold, 580)).toBe(580);
  });
  it("preserves vertical, flat and very short upper-pane viewports", () => {
    expect(
      usableViewportHeight({ ...fold, orientation: "vertical" }, 480),
    ).toBe(480);
    expect(usableViewportHeight({ orientation: "none" }, 480)).toBe(480);
    expect(usableViewportHeight({ ...fold, top: 100 }, 200)).toBe(200);
  });
});
