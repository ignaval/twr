export interface Posture {
  orientation: "none" | "vertical" | "horizontal";
  state?: string;
  separating?: boolean;
  left?: number;
  top?: number;
  width?: number;
  height?: number;
  occlusionTop?: number;
  occlusions?: { left: number; top: number; width: number; height: number }[];
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

// The body's horizontal safe area may already exclude a corner camera. Add top
// space only for occlusions that still intersect that usable horizontal span.
// Retain the native conservative inset if an older bridge lacks exact regions.
export function topOcclusionInset(
  posture: Posture,
  left: number,
  right: number,
) {
  const fallback = Math.max(0, posture.occlusionTop || 0);
  const regions = posture.occlusions;
  if (
    !regions ||
    !Number.isFinite(left) ||
    !Number.isFinite(right) ||
    right <= left
  )
    return fallback;
  if (
    regions.some(
      (region) =>
        ![region.left, region.top, region.width, region.height].every(
          Number.isFinite,
        ) ||
        region.width < 0 ||
        region.height < 0,
    )
  )
    return fallback;
  return Math.max(
    0,
    ...regions
      .filter(
        (region) =>
          region.width > 0 &&
          region.height > 0 &&
          region.left < right &&
          region.left + region.width > left,
      )
      .map((region) => region.top + region.height),
  );
}
