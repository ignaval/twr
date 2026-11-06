import type { Attention } from "./types";
export function canRespond(item: Attention, busy: boolean): boolean {
  return (
    !busy &&
    !item.incomplete &&
    Number.isSafeInteger(item.revision) &&
    item.revision > 0
  );
}
export function approvalShortcut(
  item: Attention,
  busy: boolean,
  event: {
    key: string;
    ctrlKey: boolean;
    metaKey: boolean;
    altKey: boolean;
    shiftKey: boolean;
  },
): string | undefined {
  if (
    !canRespond(item, busy) ||
    event.ctrlKey ||
    event.metaKey ||
    event.altKey ||
    event.shiftKey ||
    !/^[1-9]$/.test(event.key)
  )
    return;
  return item.options?.[Number(event.key) - 1];
}
