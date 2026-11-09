import type { MenuItemConstructorOptions } from "electron";

/** The parts of Electron's context-menu parameters the menu depends on. */
export interface ContextMenuParams {
  isEditable: boolean;
  selectionText: string;
  linkURL: string;
  misspelledWord: string;
  dictionarySuggestions: string[];
  editFlags: {
    canCut: boolean;
    canCopy: boolean;
    canPaste: boolean;
    canSelectAll: boolean;
  };
}

export interface ContextMenuActions {
  copyText(text: string): void;
  replaceMisspelling(word: string): void;
}

/**
 * The window's right-click menu: spelling suggestions, Copy Link Address (a
 * link is copied, never opened), then the editing commands. Items use Electron's
 * roles, so their labels are localized and they act on the focused page.
 */
export function contextMenuTemplate(
  params: ContextMenuParams,
  actions: ContextMenuActions,
): MenuItemConstructorOptions[] {
  const groups: MenuItemConstructorOptions[][] = [];
  if (params.isEditable && params.misspelledWord) {
    const suggestions = params.dictionarySuggestions.slice(0, 5);
    groups.push(
      suggestions.length
        ? suggestions.map((word) => ({
            label: word,
            click: () => actions.replaceMisspelling(word),
          }))
        : [{ label: "No Guesses Found", enabled: false }],
    );
  }
  if (params.linkURL && params.linkURL.length <= 8192)
    groups.push([
      {
        label: "Copy Link Address",
        click: () => actions.copyText(params.linkURL),
      },
    ]);
  const flags = params.editFlags;
  if (params.isEditable)
    groups.push(
      [
        { role: "cut", enabled: flags.canCut },
        { role: "copy", enabled: flags.canCopy },
        { role: "paste", enabled: flags.canPaste },
      ],
      [{ role: "selectAll", enabled: flags.canSelectAll }],
    );
  else if (params.selectionText.trim())
    groups.push([
      { role: "copy", enabled: flags.canCopy },
      { role: "selectAll", enabled: flags.canSelectAll },
    ]);
  return groups.flatMap((group, index) =>
    index ? [{ type: "separator" as const }, ...group] : group,
  );
}
