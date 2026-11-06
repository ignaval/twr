import { describe, it, expect } from "vitest";
import {
  topOcclusionInset,
  usableViewportHeight,
} from "../mobile/src/geometry";
describe("folded keyboard layout", () => {
  const fold = {
    orientation: "horizontal" as const,
    state: "half-opened",
    top: 400,
    height: 20,
  };
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
    expect(
      usableViewportHeight({ ...fold, state: "flat", separating: false }, 480),
    ).toBe(480);
    expect(
      usableViewportHeight({ ...fold, state: "flat", separating: true }, 480),
    ).toBe(400);

    expect(usableViewportHeight({ ...fold, top: 100 }, 200)).toBe(200);
  });
});

describe("camera occlusions and horizontal safe areas", () => {
  const corner = { left: 382, top: 0, width: 84, height: 174 };
  const posture = {
    orientation: "none" as const,
    occlusionTop: 174,
    safeAreaInsets: { top: 0, left: 0, right: 84, bottom: 0 },
    occlusions: [corner],
  };
  it("does not reserve a corner twice when the horizontal safe area already excludes it", () => {
    expect(topOcclusionInset(posture, 0, 382)).toBe(0);
    expect(
      topOcclusionInset(
        { ...posture, occlusions: [{ ...corner, left: 0 }] },
        84,
        466,
      ),
    ).toBe(0);
  });
  it("retains the entire camera inset whenever any part still overlaps usable width", () => {
    expect(topOcclusionInset(posture, 0, 383)).toBe(174);
    expect(topOcclusionInset(posture, 0, 466)).toBe(174);
  });
  it("keeps full-width occlusions clear regardless of side insets", () => {
    expect(
      topOcclusionInset(
        { ...posture, occlusions: [{ ...corner, left: 0, width: 466 }] },
        84,
        382,
      ),
    ).toBe(174);
  });
  it("accounts for every region remaining inside usable width", () => {
    expect(
      topOcclusionInset(
        {
          ...posture,
          occlusions: [corner, { left: 100, top: 0, width: 50, height: 80 }],
        },
        0,
        382,
      ),
    ).toBe(80);
  });
  it("retains the conservative native inset for legacy or invalid geometry", () => {
    expect(
      topOcclusionInset({ orientation: "none", occlusionTop: 174 }, 0, 382),
    ).toBe(174);
    expect(topOcclusionInset(posture, 0, 0)).toBe(174);
    expect(
      topOcclusionInset(
        { ...posture, occlusions: [{ ...corner, width: NaN }] },
        0,
        382,
      ),
    ).toBe(174);
  });
  it("does not turn a landscape bottom camera into a full-height top inset", () => {
    expect(
      topOcclusionInset(
        {
          ...posture,
          occlusions: [{ left: 50, top: 550, width: 84, height: 100 }],
        },
        0,
        466,
      ),
    ).toBe(0);
  });
  it("recognizes top regions relative to the native safe area", () => {
    expect(
      topOcclusionInset(
        {
          ...posture,
          safeAreaInsets: { top: 62, left: 0, right: 0, bottom: 0 },
          occlusions: [{ left: 100, top: 50, width: 84, height: 100 }],
        },
        0,
        466,
      ),
    ).toBe(150);
    expect(
      topOcclusionInset({ ...posture, safeAreaInsets: undefined }, 0, 382),
    ).toBe(174);
  });
  it("uses no extra margin when the native bridge reports no occlusion", () => {
    expect(topOcclusionInset({ ...posture, occlusions: [] }, 0, 466)).toBe(0);
  });
});
