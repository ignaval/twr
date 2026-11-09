import type { AttentionSummary } from "../shared/types";

/**
 * What the tray shows for an attention summary. The renderer sends a summary
 * on every change to its thread list, mostly with nothing new for the tray;
 * the tray is rebuilt only when this key changes.
 */
export function attentionKey(summary: AttentionSummary) {
  return JSON.stringify([
    summary.count,
    summary.items
      .slice(0, 5)
      .map((item) => [item.thread, item.title, item.summary]),
  ]);
}
