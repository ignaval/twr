import { describe, expect, it, vi } from "vitest";
import type { Json } from "../shared/types";
import {
  focusPart,
  initialSettingsTab,
  latestWrites,
  managePage,
  navSection,
  runtimeChoices,
  runtimeInstalled,
  settingsTabs,
  speechRateChoices,
  summaryEveryChoices,
  tabForSection,
  withCurrent,
  type Section,
} from "../shared/settingsModel";
import { textSizeChoices } from "../shared/layoutPreferences";
import { orchestratorSwitch } from "../shared/OrchestratorSettings";
import { FakeEvent, installFakeDOM, type FakeElement } from "./fake-dom";

/* Settings in tabs and the merged Manage pages (docs/desktop.md): which tab a section opens
 * and which nav entry it lights, the values the New thread defaults offer in words, and,
 * mounted on the stand-in DOM, that a setting saves when it changes and a box's other
 * actions sit in a menu. */

describe("sections, pages and tabs", () => {
  it("maps each section to the nav entry it highlights", () => {
    const cases: [Section, Section][] = [
      ["boxes", "accounts"],
      ["accounts", "accounts"],
      ["usage", "jobs"],
      ["jobs", "jobs"],
      ["speech", "settings"],
      ["maintenance", "settings"],
      ["settings", "settings"],
      ["memory", "memory"],
      ["trash", "trash"],
      ["chat", "chat"],
    ];
    for (const [section, nav] of cases) expect(navSection(section)).toBe(nav);
  });

  it("merges Boxes into Machines & accounts and Usage into Activity", () => {
    expect(managePage("accounts")?.title).toBe("Machines & accounts");
    expect(managePage("boxes")).toBe(managePage("accounts"));
    expect(managePage("accounts")?.parts).toEqual(["accounts", "boxes"]);
    expect(managePage("jobs")?.title).toBe("Activity");
    expect(managePage("usage")?.parts).toEqual(["jobs", "usage"]);
    expect(managePage("trash")?.parts).toEqual(["trash"]);
    expect(managePage("settings")).toBeUndefined();
    expect(managePage("speech")).toBeUndefined();
    expect(focusPart("boxes")).toBe("boxes");
    expect(focusPart("usage")).toBe("usage");
    expect(focusPart("accounts")).toBeUndefined();
  });

  it("shows a device without admin only what applies to it", () => {
    const ids = (admin: boolean, voice: boolean) =>
      settingsTabs(admin, voice).map((t) => t.id);
    expect(ids(true, false)).toEqual([
      "general",
      "agents",
      "loops",
      "voice",
      "advanced",
    ]);
    expect(ids(false, true)).toEqual(["general", "loops", "voice", "advanced"]);
    expect(ids(false, false)).toEqual(["general", "loops", "advanced"]);
  });

  it("opens Read aloud on Voice, Hub & storage on Advanced, else the last tab", () => {
    const all = settingsTabs(true, true);
    expect(tabForSection("speech")).toBe("voice");
    expect(tabForSection("maintenance")).toBe("advanced");
    expect(initialSettingsTab("speech", "loops", all)).toBe("voice");
    expect(initialSettingsTab("maintenance", null, all)).toBe("advanced");
    expect(initialSettingsTab("settings", "loops", all)).toBe("loops");
    expect(initialSettingsTab("settings", null, all)).toBe("general");
    expect(initialSettingsTab("settings", "bogus", all)).toBe("general");
    // A tab this device can't see falls back to General.
    const device = settingsTabs(false, false);
    expect(initialSettingsTab("settings", "agents", device)).toBe("general");
    expect(initialSettingsTab("speech", null, device)).toBe("general");
  });
});

describe("the New thread defaults' choices", () => {
  it("offers the summary interval in minutes, keeping another value", () => {
    expect(summaryEveryChoices("15m", 900).value).toBe("15m");
    expect(summaryEveryChoices("15m", 900).choices.map((c) => c.label)).toEqual(
      ["1 minute", "5 minutes", "15 minutes", "30 minutes"],
    );
    // The same length spelled another way selects the preset.
    expect(summaryEveryChoices("300s", 300).value).toBe("5m");
    // Unset: the hub's default.
    expect(summaryEveryChoices(undefined, undefined).value).toBe("5m");
    const odd = summaryEveryChoices("2h", 7200);
    expect(odd.value).toBe("2h");
    expect(odd.choices.at(-1)).toEqual({ value: "2h", label: "2 hours" });
  });

  it("lists the runtimes the hub found, keeping the current one", () => {
    const info = {
      agents: [
        { runtime: "claude-code", found: true },
        { runtime: "grok-build", found: false },
      ],
    };
    const show = {
      runtimes: ["claude-code", "codex", "gemini"],
      installed: { "claude-code": true, codex: true, gemini: false },
    };
    expect(runtimeChoices(info, show, "claude-code")).toEqual([
      "claude-code",
      "codex",
    ]);
    expect(runtimeChoices(info, show, "acp:custom")).toEqual([
      "claude-code",
      "codex",
      "acp:custom",
    ]);
    // Those the file had before stay, after the hub's.
    expect(runtimeChoices(info, show, "codex", ["fake", "codex"])).toEqual([
      "claude-code",
      "codex",
      "fake",
    ]);
    // An older hub lists nothing: the picker is free text.
    expect(runtimeChoices({}, undefined, "claude-code")).toEqual([]);
    expect(runtimeInstalled("codex", info, show)).toBe(true);
    expect(runtimeInstalled("gemini", info, show)).toBe(false);
    expect(runtimeInstalled("grok-build", info, undefined)).toBe(false);
    expect(runtimeInstalled("claude-code", info, undefined)).toBe(true);
  });

  it("keeps a value the config names that the list lacks", () => {
    const list = [{ value: "a", label: "A" }];
    expect(withCurrent(list, "a")).toBe(list);
    expect(withCurrent(list, "b").at(-1)).toEqual({ value: "b", label: "b" });
    expect(speechRateChoices(180).map((c) => c.value)).toContain("180");
    expect(speechRateChoices(200).length).toBe(6);
    // The voice's own speed (rate 0) is a choice while the speaker reads at it.
    expect(speechRateChoices(0)[0]).toEqual({
      value: "0",
      label: "Default (the voice's own speed)",
    });
    expect(speechRateChoices(200).map((c) => c.value)).not.toContain("0");
  });

  it("names text sizes, keeping an earlier slider's size as custom", () => {
    expect(textSizeChoices("textSize", 13).map((c) => c.label)).toEqual([
      "Small",
      "Default",
      "Large",
      "Extra large",
    ]);
    expect(textSizeChoices("textSize", 14).map((c) => c.label)).toEqual([
      "Small",
      "Default",
      "Custom (14 px)",
      "Large",
      "Extra large",
    ]);
    expect(textSizeChoices("codeSize", 24).at(-1)).toEqual({
      value: "24",
      label: "Extra large",
    });
  });

  it("switches the orchestrator with a summary, keeping its account and effort", () => {
    const show = {
      runtime: "claude-code",
      account: "work",
      account_shown: true,
      effort: "high",
    };
    expect(orchestratorSwitch(show, "claude-code", "opus")).toEqual({
      runtime: "claude-code:work",
      model: "opus",
      effort: "high",
      summary: true,
    });
    expect(orchestratorSwitch(show, "codex", "")).toEqual({
      runtime: "codex",
      summary: true,
    });
  });
});

/** Mounts the Manager on the stand-in DOM with `rpc` and a host whose `configSet` is
 * recorded. */
async function mountManager(
  props: Json,
  rpc: (method: string, params?: Json) => Promise<Json>,
  /** Settings the config file has besides the defaults below (read each time). */
  more: Json = {},
) {
  vi.resetModules();
  const document = installFakeDOM();
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  const configSet = vi.fn(async () => {});
  (window as any).twr = {
    configRead: async () => ({
      content: "",
      hash: "h",
      path: "/c/config.toml",
      settings: {
        default_runtime: "claude-code",
        threads: { default_mode: "auto", worktree: true },
        tui: { suggest: true, summaries: false, card_order: "stable" },
        stall_after_secs: 600,
        trash: { keep_days: 7 },
        log: { retention_days: 30, archive: true },
        background_timeout_secs: 86400,
        wake_after_secs: 30,
        orchestrator: { autostart: false },
        codex: { auto: "auto_review" },
        ...more,
      },
    }),
    configSet,
    // Offline history waits on this device's storage: never, here.
    bootstrap: () => new Promise(() => {}),
  };
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const [managers, editors] = await Promise.all([
    import("../shared/Managers"),
    import("../shared/editors"),
  ]);
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const settle = () =>
    act(async () => {
      for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
    });
  const base = {
    workspace: "default",
    refreshKey: 0,
    rpc,
    action: () => {},
    cliAction: () => {},
    fail: (e: unknown) => {
      throw e;
    },
    canAdmin: true,
    connected: true,
  };
  await act(async () =>
    root.render(
      createElement(
        editors.EditorSessionsProvider,
        null,
        createElement(managers.Manager, { ...base, ...props }),
      ),
    ),
  );
  await settle();
  const all = (selector: string, text?: string) =>
    document
      .querySelectorAll(selector)
      .filter(
        (node) => text === undefined || node.textContent.includes(text),
      ) as FakeElement[];
  const dispatch = async (node: FakeElement, event: FakeEvent) => {
    await act(async () => void node.dispatchEvent(event));
    await settle();
  };
  return {
    document,
    configSet,
    /** Runs `task` in act, then lets what it set off settle. */
    run: async (task: () => void) => {
      await act(async () => task());
      await settle();
    },
    all,
    find: (selector: string, text?: string) => all(selector, text)[0],
    click: (node: FakeElement) =>
      dispatch(
        node,
        new FakeEvent("click", { bubbles: true, cancelable: true, button: 0 }),
      ),
    key: (node: FakeElement, key: string) =>
      dispatch(
        node,
        new FakeEvent("keydown", { bubbles: true, cancelable: true, key }),
      ),
    choose: (node: FakeElement, value: string) => {
      (node as any).value = value;
      return dispatch(node, new FakeEvent("change", { bubbles: true }));
    },
    unmount: () => act(async () => root.unmount()),
  };
}

const hubRpc = async (method: string): Promise<Json> => {
  if (method === "config.show") return {};
  if (method === "orchestrator.show")
    return {
      runtime: "claude-code",
      model: "",
      runtimes: ["claude-code", "codex"],
      installed: { "claude-code": true, codex: true },
      models: {},
      access: "limited",
    };
  if (method === "hub.info")
    return {
      agents: [{ runtime: "claude-code", found: true }],
      boxes: [{ name: "devbox", state: "online", online: true, agents: [] }],
    };
  if (method === "account.list")
    return {
      runtimes: {
        "claude-code": { accounts: [{ name: "default", native: true }] },
      },
    };
  if (method === "runner.status") return {};
  throw new Error(`unexpected ${method}`);
};

describe("Settings, mounted", () => {
  it("opens on a section's tab and moves between tabs with the arrow keys", async () => {
    const m = await mountManager({ section: "maintenance" }, hubRpc);
    const selected = () =>
      m.find("[role=tab][aria-selected=true]")?.textContent;
    expect(selected()).toBe("Advanced");
    await m.key(m.find("[role=tab]", "Advanced"), "ArrowRight");
    expect(selected()).toBe("General");
    expect(m.document.activeElement?.textContent).toBe("General");
    await m.key(m.find("[role=tab]", "General"), "End");
    expect(selected()).toBe("Advanced");
    // The config file stays folded until asked for.
    expect(m.find("textarea.config-editor")).toBeUndefined();
    await m.click(m.find("button", "Edit config file…"));
    expect(m.find("textarea.config-editor")).toBeDefined();
    await m.unmount();
  });

  it("saves a New thread default when it changes, and says so", async () => {
    const m = await mountManager({ section: "settings" }, hubRpc);
    await m.click(m.find("[role=tab]", "Agents"));
    expect(m.find("h2", "New thread defaults")).toBeDefined();
    // No per-row Save button, no raw key on screen.
    expect(m.find(".setting-controls button", "Save")).toBeUndefined();
    expect(m.find(".setting-controls")!.textContent).not.toContain(
      "idle_compaction",
    );
    const suggest = m.find("label", "Suggest next messages")!;
    const toggle = m.document.getElementById(
      suggest.getAttribute("for")!,
    ) as unknown as FakeElement & { checked: boolean };
    expect(toggle.getAttribute("role")).toBe("switch");
    toggle.checked = false;
    await m.click(toggle);
    expect(m.configSet).toHaveBeenCalledWith(["tui"], "suggest", false);
    const row = m.find(".app-option", "Suggest next messages")!;
    expect(row.querySelector("[role=status]")!.textContent).toBe("Saved");
    // A hub setting says it waits for a restart.
    const stall = m.find(".setting-field", "Stall notification")!;
    const select = stall.querySelector("select")!;
    expect(select.querySelectorAll("option").map((o) => o.textContent)).toEqual(
      ["Off", "5 minutes", "10 minutes", "30 minutes", "1 hour"],
    );
    await m.choose(select, "1800");
    expect(m.configSet).toHaveBeenCalledWith([], "stall_after_secs", 1800);
    expect(stall.textContent).toContain("Applies after the hub restarts.");
    // Permission modes in words; the id is what is saved.
    const mode = m.find(".setting-field", "Default permission mode")!;
    expect(mode.querySelector("select")!.textContent).toContain(
      "Agent decides",
    );
    await m.choose(mode.querySelector("select")!, "plan");
    expect(m.configSet).toHaveBeenCalledWith(
      ["threads"],
      "default_mode",
      "plan",
    );
    await m.unmount();
  });
});

describe("New thread defaults, mounted", () => {
  it("keeps an unlisted default runtime a choice, and takes any other typed in", async () => {
    const file: Json = { default_runtime: "fake" };
    const m = await mountManager({ section: "settings" }, hubRpc, file);
    await m.click(m.find("[role=tab]", "Agents"));
    (window as any).twr.configSet = async (
      _table: string[],
      key: string,
      value: string,
    ) => {
      file[key] = value;
    };
    const row = () => m.find(".setting-field", "Default runtime")!;
    const options = () =>
      row()
        .querySelectorAll("option")
        .map((o) => o.textContent);
    expect(options()).toEqual(["claude-code", "codex", "fake", "Other…"]);
    await m.choose(row().querySelector("select")!, "codex");
    expect(file.default_runtime).toBe("codex");
    // The runtime the file had before stays a choice.
    expect(options()).toEqual(["claude-code", "codex", "fake", "Other…"]);
    await m.choose(row().querySelector("select")!, "\u0000other");
    const input = row().querySelector("input")!;
    expect(input).toBeDefined();
    expect((input as any).value).toBe("codex");
    expect(
      row().querySelector(
        "button[aria-label='Choose default runtime from the list']",
      ),
    ).toBeDefined();
    await m.unmount();
  });

  it("names a setting by its label, and describes it by its hint", async () => {
    const m = await mountManager({ section: "settings" }, hubRpc);
    await m.click(m.find("[role=tab]", "Agents"));
    const stall = m.find(".setting-field", "Stall notification")!;
    const label = stall.querySelector("label")!;
    expect(label.textContent).toBe("Stall notification");
    const select = stall.querySelector("select")!;
    expect(label.getAttribute("for")).toBe(select.getAttribute("id"));
    const hint = m.document.getElementById(
      select.getAttribute("aria-describedby")!,
    )!;
    expect(hint.textContent).toBe(
      "When a running turn makes no progress this long.",
    );
    await m.unmount();
  });
});

describe("Machines & accounts, mounted", () => {
  it("shows accounts and machines on one page, a box's other actions in a menu", async () => {
    const m = await mountManager({ section: "boxes" }, hubRpc);
    expect(m.find("h1")!.textContent).toBe("Machines & accounts");
    expect(m.find("h2", "Accounts")).toBeDefined();
    expect(m.find("h2", "Machines")).toBeDefined();
    expect(m.find(".eyebrow")).toBeUndefined();
    const card = m.find(".resource-card", "devbox")!;
    const buttons = () =>
      card.querySelectorAll("button").map((b) => b.textContent);
    expect(buttons()).toContain("Diagnose");
    expect(buttons()).toContain("Sign in");
    expect(buttons()).not.toContain("Remove…");
    const more = card.querySelector("[aria-haspopup=menu]")!;
    expect(more.getAttribute("aria-label")).toBe("More actions for devbox");
    await m.click(more);
    expect(more.getAttribute("aria-expanded")).toBe("true");
    const items = m.all("[role=menuitem]").map((i) => i.textContent);
    expect(items).toEqual([
      "Install or update twr",
      "Restart…",
      "Projects",
      "Token…",
      "Remove…",
    ]);
    expect(m.document.activeElement?.textContent).toBe("Install or update twr");
    await m.key(m.find("[role=menuitem]", "Restart")!, "Escape");
    expect(m.find("[role=menu]")).toBeUndefined();
    expect(m.document.activeElement).toBe(more);
    await m.unmount();
  });
});

describe("a setting saved as it changes", () => {
  /** A writer whose writes end when the test says so. */
  const held = () => {
    const writes: { value: string; end: (error?: Error) => void }[] = [];
    let idle = 0;
    const write = latestWrites<string>(
      (value) =>
        new Promise<void>((resolve, reject) =>
          writes.push({
            value,
            end: (error) => (error ? reject(error) : resolve()),
          }),
        ),
      () => idle++,
    );
    return { writes, write, idle: () => idle };
  };
  const tick = () => new Promise((r) => setTimeout(r, 0));

  it("writes one value at a time, the latest of those waiting next", async () => {
    const { writes, write, idle } = held();
    const a = write("a");
    const b = write("b");
    const c = write("c");
    // b never starts: c replaced it while a was being written.
    expect(writes.map((w) => w.value)).toEqual(["a"]);
    expect(await b).toEqual({ status: "superseded" });
    writes[0].end();
    expect(await a).toEqual({ status: "superseded" });
    await tick();
    expect(writes.map((w) => w.value)).toEqual(["a", "c"]);
    expect(idle()).toBe(0);
    writes[1].end();
    expect(await c).toEqual({ status: "saved" });
    // The file is read again once, after the last write.
    expect(idle()).toBe(1);
  });

  it("reports the latest write's failure, and an earlier one's not at all", async () => {
    const { writes, write, idle } = held();
    const a = write("a");
    const b = write("b");
    writes[0].end(new Error("locked"));
    expect(await a).toEqual({ status: "superseded" });
    await tick();
    const failure = new Error("disk full");
    writes[1].end(failure);
    expect(await b).toEqual({ status: "failed", error: failure });
    expect(idle()).toBe(1);
    // Idle again, the next value is written at once.
    void write("c");
    expect(writes.map((w) => w.value)).toEqual(["a", "b", "c"]);
  });

  it("never lets two quick removals of project folders land out of order", async () => {
    const file: Json = { project_roots: ["/a", "/b", "/c"] };
    const m = await mountManager({ section: "settings" }, hubRpc, file);
    await m.click(m.find("[role=tab]", "Agents"));
    const writes: { value: string[]; end: () => void }[] = [];
    (window as any).twr.configSet = (
      _table: string[],
      _key: string,
      value: string[],
    ) =>
      new Promise<void>((resolve) =>
        writes.push({
          value,
          end: () => {
            file.project_roots = value;
            resolve();
          },
        }),
      );
    const shown = () =>
      m.all(".project-folders li code").map((c) => c.textContent);
    await m.click(m.find("button[aria-label='Remove /a']"));
    await m.click(m.find("button[aria-label='Remove /b']"));
    expect(shown()).toEqual(["/c"]);
    // The second list waits for the first write.
    expect(writes.map((w) => w.value)).toEqual([["/b", "/c"]]);
    expect(m.find(".project-folders [role=status]")!.textContent).toBe(
      "Saving…",
    );
    await m.run(() => writes[0].end());
    expect(writes.map((w) => w.value)).toEqual([["/b", "/c"], ["/c"]]);
    // No read of the file between the writes puts /b back on screen.
    expect(shown()).toEqual(["/c"]);
    await m.run(() => writes[1].end());
    expect(file.project_roots).toEqual(["/c"]);
    expect(shown()).toEqual(["/c"]);
    expect(m.find(".project-folders [role=status]")!.textContent).toBe("Saved");
    await m.unmount();
  });
});
