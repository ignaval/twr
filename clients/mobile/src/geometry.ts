export interface Posture {
  orientation: "none" | "vertical" | "horizontal";
  state?: string;
  separating?: boolean;
  left?: number;
  top?: number;
  width?: number;
  height?: number;
  occlusionTop?: number;
}

// When the keyboard consumes the lower pane, keep controls above the hinge.
// 160px accommodates terminal tabs, keys and a small readable terminal surface.
export function usableViewportHeight(posture: Posture, viewportHeight: number) {
  const top = posture.top || 0;
  const bottom = top + (posture.height || 0);
  return posture.orientation === "horizontal" &&
    (posture.separating || posture.state === "half-opened") &&
    top >= 180 &&
    viewportHeight - bottom < 160
    ? Math.min(viewportHeight, top)
    : viewportHeight;
}
