import { describe, expect, it } from "vitest";
import { attentionKey, trayEntries } from "../desktop/tray-state";
import type { AttentionSummary } from "../shared/types";

const item = (
  thread: string,
  summary = "Needs approval",
  workspace = "default",
) => ({
  workspace,
  thread,
  title: `Thread ${thread}`,
  summary,
  id: `attn-${workspace}-${thread}`,
  revision: 1,
  reason: "permission",
});
const summary = (
  items: ReturnType<typeof item>[],
  count = items.length,
  workspaces = ["default"],
): AttentionSummary => ({
  count,
  items,
  workspaces: workspaces.map((name) => ({ name, stream: "s" })),
});

describe("tray attention key", () => {
  it("is stable for a repeated summary and changes with anything the tray shows", () => {
    const base = summary([item("a"), item("b")]);
    const key = attentionKey(base);
    expect(attentionKey(summary([item("a"), item("b")]))).toBe(key);
    expect(attentionKey({ ...base, count: 3 })).not.toBe(key);
    expect(attentionKey(summary([item("b"), item("a")]))).not.toBe(key);
    expect(attentionKey(summary([item("a"), item("b", "Asked")]))).not.toBe(
      key,
    );
    expect(
      attentionKey(summary([item("a"), item("b", "Needs approval", "side")])),
    ).not.toBe(key);
  });

  it("ignores items past the five the menu lists, and what the menu doesn't show", () => {
    const five = Array.from({ length: 5 }, (_, index) => item(String(index)));
    expect(attentionKey(summary([...five, item("x")], 9))).toBe(
      attentionKey(summary([...five, item("y")], 9)),
    );
    // A new revision or stream changes nothing in the menu.
    const base = summary([item("a")]);
    expect(
      attentionKey({
        ...base,
        items: [{ ...item("a"), revision: 2 }],
        workspaces: [{ name: "default", stream: "other" }],
      }),
    ).toBe(attentionKey(base));
  });
});

describe("tray entries", () => {
  it("lists one workspace's items without headings", () => {
    const { entries, more } = trayEntries(summary([item("a")]));
    expect(entries).toEqual([
      {
        kind: "item",
        label: "Thread a: Needs approval",
        workspace: "default",
        thread: "a",
      },
    ]);
    expect(more).toBe(false);
  });

  it("groups items under their workspace when there are several", () => {
    const { entries, more } = trayEntries(
      summary(
        [
          item("a"),
          item("b", "Asked", "client-x"),
          item("c", "Run tests?", "client-x"),
        ],
        7,
        ["default", "client-x"],
      ),
    );
    expect(entries.map((entry) => [entry.kind, entry.label])).toEqual([
      ["workspace", "default"],
      ["item", "Thread a: Needs approval"],
      ["workspace", "client-x"],
      ["item", "Thread b: Asked"],
      ["item", "Thread c: Run tests?"],
    ]);
    expect(entries[3]).toMatchObject({ workspace: "client-x", thread: "b" });
    expect(more).toBe(true);
  });

  it("shortens long labels to one line", () => {
    const { entries } = trayEntries(
      summary([item("a", "line one\nline two " + "x".repeat(80))]),
    );
    expect(entries[0].label).toHaveLength(60);
    expect(entries[0].label).not.toContain("\n");
    expect(entries[0].label.endsWith("…")).toBe(true);
  });
});
