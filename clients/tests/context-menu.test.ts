import { describe, expect, it, vi } from "vitest";
import {
  contextMenuTemplate,
  type ContextMenuParams,
} from "../desktop/context-menu";

const params = (overrides: Partial<ContextMenuParams>): ContextMenuParams => ({
  isEditable: false,
  selectionText: "",
  linkURL: "",
  misspelledWord: "",
  dictionarySuggestions: [],
  editFlags: {
    canCut: false,
    canCopy: false,
    canPaste: false,
    canSelectAll: true,
  },
  ...overrides,
});
const actions = () => ({ copyText: vi.fn(), replaceMisspelling: vi.fn() });
const shape = (items: ReturnType<typeof contextMenuTemplate>) =>
  items.map((item) =>
    item.type === "separator"
      ? "-"
      : `${item.role || item.label}${item.enabled === false ? " (off)" : ""}`,
  );

describe("context menu", () => {
  it("offers editing commands in text fields, enabled by the edit flags", () => {
    const menu = contextMenuTemplate(
      params({
        isEditable: true,
        editFlags: {
          canCut: true,
          canCopy: true,
          canPaste: false,
          canSelectAll: true,
        },
      }),
      actions(),
    );
    expect(shape(menu)).toEqual([
      "cut",
      "copy",
      "paste (off)",
      "-",
      "selectAll",
    ]);
  });

  it("offers Copy for selected page text and nothing for an empty click", () => {
    expect(
      shape(
        contextMenuTemplate(
          params({
            selectionText: "hello",
            editFlags: {
              canCut: false,
              canCopy: true,
              canPaste: false,
              canSelectAll: true,
            },
          }),
          actions(),
        ),
      ),
    ).toEqual(["copy", "selectAll"]);
    expect(contextMenuTemplate(params({}), actions())).toEqual([]);
  });

  it("copies a link's address and never opens it", () => {
    const callbacks = actions();
    const menu = contextMenuTemplate(
      params({ linkURL: "https://example.com/a?b=c", selectionText: "a" }),
      callbacks,
    );
    expect(shape(menu)).toEqual([
      "Copy Link Address",
      "-",
      "copy (off)",
      "selectAll",
    ]);
    (menu[0].click as () => void)();
    expect(callbacks.copyText).toHaveBeenCalledWith(
      "https://example.com/a?b=c",
    );
    expect(menu.some((item) => item.role === ("open" as never))).toBe(false);
  });

  it("puts spelling suggestions first in a text field", () => {
    const callbacks = actions();
    const menu = contextMenuTemplate(
      params({
        isEditable: true,
        misspelledWord: "teh",
        dictionarySuggestions: ["the", "ten", "tea", "tee", "eh", "toe"],
      }),
      callbacks,
    );
    expect(shape(menu).slice(0, 6)).toEqual([
      "the",
      "ten",
      "tea",
      "tee",
      "eh",
      "-",
    ]);
    (menu[0].click as () => void)();
    expect(callbacks.replaceMisspelling).toHaveBeenCalledWith("the");
    expect(
      shape(
        contextMenuTemplate(
          params({ isEditable: true, misspelledWord: "zzz" }),
          callbacks,
        ),
      )[0],
    ).toBe("No Guesses Found (off)");
  });
});
