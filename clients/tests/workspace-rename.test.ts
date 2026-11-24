import { describe, expect, it } from "vitest";
import { localInboxSources } from "../desktop/inbox";
import { runsWithoutWorkspace } from "../desktop/validation";
import { notices } from "../desktop/notify-state";
import { trayEntries } from "../desktop/tray-state";
import { attentionSummary } from "../shared/attentionSummary";
import type { InboxSource } from "../shared/types";
import {
  labelOf,
  resolveWorkspace,
  startWorkspace,
  workspaceEntries,
} from "../shared/workspaceTree";

// `twr ws list --json` after `twr ws rename default main` and `twr ws new acme` renamed to
// `client`: rows carry the id that selects each workspace and its hub reports, and its name.
const listing = [
  { id: "default", name: "main", default: true, hub: "running" },
  { id: "acme", name: "client", default: false, hub: "running" },
];
const source = (workspace: string, extra: Partial<InboxSource> = {}) =>
  ({
    hubId: "h",
    hubName: "Hub",
    workspace,
    attention: [],
    threads: [],
    ...extra,
  }) as InboxSource;

describe("a renamed workspace", () => {
  it("is one sidebar entry, keyed by its id and shown by its name", () => {
    const entries = workspaceEntries(
      listing,
      "acme",
      "h",
      // The gateway's sources and the hubs name workspaces by id.
      [
        source("default", {
          attention: [{ attn_id: "q", thread: "t" } as never],
        }),
        source("acme"),
      ],
      undefined,
    );
    expect(entries.map((e) => [e.id, e.name])).toEqual([
      ["default", "main"],
      ["acme", "client"],
    ]);
    expect(entries[0]).toMatchObject({ waiting: 1, current: false });
    expect(entries[1].current).toBe(true);
  });

  it("is selected by its name or its id, and shown by its name", () => {
    expect(resolveWorkspace(listing, "main")).toBe("default");
    expect(resolveWorkspace(listing, "default")).toBe("default");
    expect(resolveWorkspace(listing, "client")).toBe("acme");
    expect(resolveWorkspace(listing, "acme")).toBe("acme");
    expect(resolveWorkspace(listing, "nope")).toBeUndefined();
    expect(labelOf(listing, "acme")).toBe("client");
    expect(labelOf(listing, "gone")).toBe("gone");
  });

  it("is read on the desktop by its id, which its hub reports", async () => {
    const sources = await localInboxSources(
      [{ id: "acme", name: "client", hub: "running" }],
      (workspace) => ({
        close: () => {},
        call: async (method: string) =>
          method === "hub.info"
            ? { workspace }
            : method === "attention.list"
              ? []
              : [],
      }),
    );
    expect(sources).toHaveLength(1);
    expect(sources[0]).toMatchObject({
      workspace: "acme",
      workspaceName: "client",
    });
    expect(sources[0].error).toBeUndefined();
  });

  it("is named by its name in the tray and in notifications", () => {
    const summary = attentionSummary({
      current: "default",
      currentName: "main",
      hubId: "h",
      live: { threads: [], attention: [] },
      sources: [
        source("acme", {
          workspaceName: "client",
          threads: [{ id: "t", name: "api" } as never],
          attention: [
            { attn_id: "p", thread: "t", reason: "permission" } as never,
          ],
        }),
      ],
    });
    expect(summary.items[0]).toMatchObject({
      workspace: "acme",
      workspaceName: "client",
    });
    const { entries } = trayEntries(summary);
    expect(entries[0]).toEqual({ kind: "workspace", label: "client" });
    const [notice] = notices(summary.items);
    expect(notice.title).toContain("client");
    expect(notice.target).toEqual({ workspace: "acme", thread: "t" });
  });

  it("opens the desktop on the default workspace, whatever it is called", () => {
    // `TWR_WORKSPACE` wins, as with `twr`.
    expect(startWorkspace("acme", "default", listing)).toBe("acme");
    // The one shown last, by its id, while it is there.
    expect(startWorkspace(undefined, "client", listing)).toBe("acme");
    expect(startWorkspace(undefined, "acme", listing)).toBe("acme");
    // Else the default one: after `twr ws rm default` and `twr ws default acme`, not `default`.
    const later = [{ id: "acme", name: "client", default: true }];
    expect(startWorkspace(undefined, "default", later)).toBe("acme");
    expect(startWorkspace(undefined, undefined, later)).toBe("acme");
    // No default workspace (it was removed, none chosen since): the first listed, as a bare
    // `twr` opens.
    expect(
      startWorkspace(undefined, "default", [
        { id: "a", name: "a", default: false },
      ]),
    ).toBe("a");
    expect(startWorkspace(undefined, undefined, [])).toBe("default");
    // A listing that could not be read: the saved one, else `default`.
    expect(startWorkspace(undefined, "acme", undefined)).toBe("acme");
    expect(startWorkspace(undefined, undefined, undefined)).toBe("default");
  });

  it("leaves a desktop whose workspace could not be opened able to list and make others", () => {
    expect(runsWithoutWorkspace(["ws", "list", "--json"])).toBe(true);
    expect(runsWithoutWorkspace(["ws", "list", "--json", "--detail"])).toBe(
      true,
    );
    expect(runsWithoutWorkspace(["ws"])).toBe(true);
    expect(runsWithoutWorkspace(["ws", "new", "b"])).toBe(true);
    // Not what changes or removes one, nor anything else.
    expect(runsWithoutWorkspace(["ws", "rm", "b"])).toBe(false);
    expect(runsWithoutWorkspace(["ws", "rename", "a", "b"])).toBe(false);
    expect(runsWithoutWorkspace(["thread", "list"])).toBe(false);
  });
});
