import type { AttentionSummary } from "../shared/types";

/** How many waiting items the tray menu lists. */
export const TRAY_ITEMS = 5;

export type TrayEntry =
  | { kind: "workspace"; label: string }
  | { kind: "item"; label: string; workspace: string; thread: string };

export function truncate(text: string, length: number) {
  const single = text.replace(/\s+/g, " ").trim();
  return single.length > length ? single.slice(0, length - 1) + "…" : single;
}

/**
 * The tray menu's list of waiting items: the first {@link TRAY_ITEMS} of the summary, under
 * the name of their workspace when the summary covers more than one, and whether there are
 * more than it lists.
 */
export function trayEntries(
  summary: AttentionSummary,
  max = TRAY_ITEMS,
): { entries: TrayEntry[]; more: boolean } {
  const shown = summary.items.slice(0, max);
  const grouped = summary.workspaces.length > 1;
  const entries: TrayEntry[] = [];
  let heading: string | undefined;
  for (const item of shown) {
    if (grouped && item.workspace !== heading) {
      heading = item.workspace;
      entries.push({ kind: "workspace", label: item.workspace });
    }
    entries.push({
      kind: "item",
      label: truncate(`${item.title}: ${item.summary}`, 60),
      workspace: item.workspace,
      thread: item.thread,
    });
  }
  return { entries, more: summary.count > shown.length };
}

/**
 * What the tray shows for an attention summary. The renderer sends a summary
 * on every change to its thread lists, mostly with nothing new for the tray;
 * the tray is rebuilt only when this key changes.
 */
export function attentionKey(summary: AttentionSummary) {
  return JSON.stringify([summary.count, trayEntries(summary)]);
}
