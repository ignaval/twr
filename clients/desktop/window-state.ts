import { z } from "zod";

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export const DEFAULT_WINDOW = { width: 1360, height: 900 };
export const MIN_WINDOW = { width: 420, height: 480 };

const coordinate = z.number().int().min(-1_000_000).max(1_000_000);
const length = z.number().int().min(1).max(1_000_000);
/** The window's normal (unmaximized) bounds and state, saved in desktop.json. */
export const windowStateSchema = z
  .object({
    x: coordinate,
    y: coordinate,
    width: length,
    height: length,
    maximized: z.boolean(),
    fullscreen: z.boolean(),
  })
  .strict();
export type WindowState = z.infer<typeof windowStateSchema>;

export interface RestoredWindow {
  /** No x/y means the platform's default placement. */
  bounds: { width: number; height: number; x?: number; y?: number };
  maximized: boolean;
  fullscreen: boolean;
}

function overlap(a: Rect, b: Rect) {
  return {
    width: Math.max(
      0,
      Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x),
    ),
    height: Math.max(
      0,
      Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y),
    ),
  };
}

const clamp = (value: number, low: number, high: number) =>
  Math.min(Math.max(value, low), Math.max(low, high));

/**
 * Where to open the window, from its saved state and the displays' work areas
 * (the primary display first). A window is put back on the display that shows
 * most of it, made to fit that display and moved fully onto it. When no display
 * shows a usable part of it (that monitor is gone), it keeps its size but gets
 * the default placement.
 */
export function restoreWindow(saved: unknown, areas: Rect[]): RestoredWindow {
  const state = windowStateSchema.safeParse(saved);
  if (!state.success)
    return {
      bounds: { ...DEFAULT_WINDOW },
      maximized: false,
      fullscreen: false,
    };
  const { maximized, fullscreen, ...bounds } = state.data;
  let best: Rect | undefined;
  let bestArea = 0;
  for (const area of areas) {
    const shared = overlap(bounds, area);
    // Enough to see and grab the title bar.
    if (
      shared.width < Math.min(100, bounds.width) ||
      shared.height < Math.min(50, bounds.height)
    )
      continue;
    if (shared.width * shared.height > bestArea) {
      best = area;
      bestArea = shared.width * shared.height;
    }
  }
  const fit = (area: Rect | undefined) => ({
    width: clamp(
      bounds.width,
      MIN_WINDOW.width,
      area ? area.width : bounds.width,
    ),
    height: clamp(
      bounds.height,
      MIN_WINDOW.height,
      area ? area.height : bounds.height,
    ),
  });
  if (!best) return { bounds: fit(areas[0]), maximized, fullscreen };
  const size = fit(best);
  return {
    bounds: {
      ...size,
      x: clamp(bounds.x, best.x, best.x + best.width - size.width),
      y: clamp(bounds.y, best.y, best.y + best.height - size.height),
    },
    maximized,
    fullscreen,
  };
}
