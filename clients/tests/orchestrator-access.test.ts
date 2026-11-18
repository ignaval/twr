import { describe, expect, it, vi } from "vitest";
import type { HubEvent, Json } from "../shared/types";
import {
  FULL_ACCESS_CONFIRM,
  accessLevel,
  accessOutcome,
} from "../shared/orchestrator-access";
import { commands, confirmationOf, parameters } from "../shared/commands";
import { blocksOf } from "../shared/transcript";
import { FakeEvent, installFakeDOM, type FakeElement } from "./fake-dom";

/* The orchestrator's access level (`[orchestrator] access`, `orchestrator.access`) in the
 * shared UI: Settings › Orchestrator's choice, mounted on the stand-in DOM with the hub
 * answering by a mock `rpc`; the command palette's entry; what each answer says; the
 * transcript's line for the `config.changed` it leaves on the orchestrator thread. */

type Rpc = (method: string, params?: Json) => Promise<Json>;

async function settle(act: (fn: () => Promise<void>) => Promise<void>) {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
  });
}

/** Mounts `render(module, createElement)` and returns a way to find and click in it. */
async function mountWith(
  load: () => Promise<Json>,
  render: (module: Json, h: Json) => Json,
) {
  vi.resetModules();
  const document = installFakeDOM();
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const module = await load();
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  await act(async () => root.render(render(module, createElement)));
  await settle(act);
  const all = (selector: string, text?: string) =>
    document
      .querySelectorAll(selector)
      .filter(
        (node) => text === undefined || node.textContent.includes(text),
      ) as FakeElement[];
  return {
    document,
    all,
    find: (selector: string, text?: string) => all(selector, text)[0],
    text: () => document.body.textContent,
    click: async (node: FakeElement) => {
      await act(
        async () =>
          void node.dispatchEvent(
            new FakeEvent("click", {
              bubbles: true,
              cancelable: true,
              button: 0,
            }),
          ),
      );
      await settle(act);
    },
    rerender: async (element: (module: Json, h: Json) => Json) => {
      await act(async () => root.render(element(module, createElement)));
      await settle(act);
    },
    unmount: () => act(async () => root.unmount()),
  };
}

/** The control alone, the hub reporting `shown` from `orchestrator.show` and answering
 * `orchestrator.access` with `answer`. */
async function mount(
  props: { live?: unknown } = {},
  {
    shown = {},
    answer = {},
  }: { shown?: Json; answer?: Json | ((params: Json) => Json) } = {},
) {
  const calls: [string, Json][] = [];
  const changed = vi.fn();
  const rpc: Rpc = async (method, params) => {
    calls.push([method, params]);
    if (method === "orchestrator.show") return shown;
    if (method === "orchestrator.access") {
      const value = typeof answer === "function" ? answer(params) : answer;
      if (value instanceof Error) throw value;
      return value;
    }
    throw new Error(`unexpected ${method}`);
  };
  const m = await mountWith(
    () => import("../shared/OrchestratorAccessOptions"),
    (v, h) => h(v.OrchestratorAccessOptions, { rpc, changed, ...props }),
  );
  return {
    ...m,
    calls,
    changed,
    accessCalls: () =>
      calls.filter(([method]) => method === "orchestrator.access"),
    choice: (label: string) => m.find(".segmented button", label)!,
  };
}

describe("Settings › Orchestrator access", () => {
  it("is hidden on a hub that reports no access level", async () => {
    const m = await mount({}, { shown: { runtime: "claude-code" } });
    expect(m.text()).not.toContain("Orchestrator access");
    expect(m.find(".segmented")).toBeUndefined();
    await m.unmount();
  });

  it("shows the level the hub reports, from hub.info or orchestrator.show", async () => {
    const fromInfo = await mount({ live: "limited" });
    expect(fromInfo.choice("Limited").getAttribute("aria-pressed")).toBe(
      "true",
    );
    expect(fromInfo.choice("Full").getAttribute("aria-pressed")).toBe("false");
    await fromInfo.unmount();
    const fromShow = await mount({}, { shown: { access: "full" } });
    expect(fromShow.choice("Full").getAttribute("aria-pressed")).toBe("true");
    // Each choice is described in one line, tied to its button.
    const full = fromShow.choice("Full");
    const description = fromShow.document.getElementById(
      full.getAttribute("aria-describedby")!,
    );
    expect(description?.textContent).toContain("bypassPermissions");
    await fromShow.unmount();
  });

  it("asks before Full and sends nothing until confirmed", async () => {
    const m = await mount(
      { live: "limited" },
      {
        answer: {
          access: "full",
          previous: "limited",
          persisted: true,
          restart: "after_turn",
        },
      },
    );
    await m.click(m.choice("Full"));
    const dialog = m.find("[role=dialog]")!;
    expect(dialog.textContent).toContain(FULL_ACCESS_CONFIRM.title);
    expect(dialog.textContent).toContain("any command");
    expect(dialog.textContent).toContain("while you may be away");
    expect(dialog.textContent).toContain("anything your account can");
    expect(dialog.textContent).toContain("current turn ends");
    expect(m.accessCalls()).toEqual([]);
    // Cancel: nothing changes.
    await m.click(m.find("[role=dialog] button", "Cancel")!);
    expect(m.find("[role=dialog]")).toBeUndefined();
    expect(m.accessCalls()).toEqual([]);
    // Confirmed: the hub is asked, and its answer is said.
    await m.click(m.choice("Full"));
    await m.click(m.find("[role=dialog] button", FULL_ACCESS_CONFIRM.action)!);
    expect(m.accessCalls()).toEqual([
      ["orchestrator.access", { access: "full" }],
    ]);
    expect(m.choice("Full").getAttribute("aria-pressed")).toBe("true");
    expect(m.find("[role=status]")?.textContent).toBe(
      "The orchestrator is in a turn. It switches to full access when that turn ends.",
    );
    expect(m.changed).toHaveBeenCalledTimes(1);
    // The level is read again after the change.
    expect(
      m.calls.filter(([method]) => method === "orchestrator.show").length,
    ).toBe(2);
    await m.unmount();
  });

  it("switches to Limited without asking", async () => {
    const m = await mount(
      { live: "full" },
      {
        answer: {
          access: "limited",
          previous: "full",
          persisted: true,
          restart: "now",
        },
      },
    );
    await m.click(m.choice("Limited"));
    expect(m.find("[role=dialog]")).toBeUndefined();
    expect(m.accessCalls()).toEqual([
      ["orchestrator.access", { access: "limited" }],
    ]);
    expect(m.choice("Limited").getAttribute("aria-pressed")).toBe("true");
    expect(m.find("[role=status]")?.textContent).toBe(
      "The orchestrator restarted with limited access. Its conversation continues.",
    );
    // The level already chosen sends nothing.
    await m.click(m.choice("Limited"));
    expect(m.accessCalls().length).toBe(1);
    await m.unmount();
  });

  it("warns when config.toml couldn't be updated", async () => {
    const m = await mount(
      { live: "full" },
      {
        answer: {
          access: "limited",
          previous: "full",
          persisted: false,
          persist_error: "permission denied",
          restart: "none",
        },
      },
    );
    await m.click(m.choice("Limited"));
    expect(m.find("[role=alert]")?.textContent).toBe(
      "config.toml couldn't be updated (permission denied), so this applies only until the hub restarts.",
    );
    await m.unmount();
  });

  it("disables itself on a hub without the method, and shows other refusals", async () => {
    const old = await mount(
      { live: "full" },
      { answer: () => new Error('unknown method "orchestrator.access"') },
    );
    await old.click(old.choice("Limited"));
    expect(old.find("[role=alert]")?.textContent).toContain(
      "This hub can't change the orchestrator's access",
    );
    for (const label of ["Limited", "Full"])
      expect(old.choice(label).getAttribute("disabled")).not.toBeNull();
    expect(old.choice("Full").getAttribute("aria-pressed")).toBe("true");
    await old.unmount();
    const refused = await mount(
      { live: "full" },
      { answer: () => new Error("this device needs the admin scope") },
    );
    await refused.click(refused.choice("Limited"));
    expect(refused.find("[role=alert]")?.textContent).toBe(
      "this device needs the admin scope",
    );
    expect(refused.choice("Full").getAttribute("aria-pressed")).toBe("true");
    expect(refused.choice("Limited").getAttribute("disabled")).toBeNull();
    await refused.unmount();
  });

  it("follows the level the app reads from hub.info", async () => {
    const m = await mount({ live: "limited" });
    const rpc: Rpc = async (method) =>
      method === "orchestrator.show" ? {} : {};
    await m.rerender((v, h) =>
      h(v.OrchestratorAccessOptions, { rpc, live: "full" }),
    );
    expect(m.choice("Full").getAttribute("aria-pressed")).toBe("true");
    await m.unmount();
  });
});

describe("Settings › Orchestrator access and the admin scope", () => {
  /** Settings, its other sections stubbed: only who sees the access choice is tested. */
  async function settings(canAdmin: boolean) {
    vi.doMock("../shared/LoopViews", () => ({
      LoopSettingsPanel: () => null,
    }));
    for (const [path, name] of [
      ["../shared/OfflineOptions", "OfflineOptions"],
      ["../shared/AppOptions", "AppOptions"],
      ["../shared/LayoutOptions", "LayoutOptions"],
      ["../shared/AppearanceOptions", "AppearanceOptions"],
    ])
      vi.doMock(path, () => ({ [name]: () => null }));
    const rpc: Rpc = async (method) => {
      if (method === "orchestrator.show") return { access: "limited" };
      if (method === "config.show") return {};
      throw new Error(`unexpected ${method}`);
    };
    const m = await mountWith(
      async () => {
        (window as any).twr = {
          configRead: async () => ({ content: "", hash: "h", path: "/c" }),
        };
        const [managers, editors] = await Promise.all([
          import("../shared/Managers"),
          import("../shared/editors"),
        ]);
        return { ...managers, ...editors };
      },
      (v, h) =>
        h(
          v.EditorSessionsProvider,
          null,
          h(v.Manager, {
            section: "settings",
            workspace: "default",
            refreshKey: 0,
            rpc,
            action: () => {},
            cliAction: () => {},
            fail: () => {},
            canAdmin,
            connected: true,
            orchestratorAccess: "limited",
          }),
        ),
    );
    vi.doUnmock("../shared/LoopViews");
    for (const path of [
      "../shared/OfflineOptions",
      "../shared/AppOptions",
      "../shared/LayoutOptions",
      "../shared/AppearanceOptions",
    ])
      vi.doUnmock(path);
    return m;
  }

  it("is offered to the user's own clients and admin devices only", async () => {
    const admin = await settings(true);
    expect(admin.text()).toContain("Orchestrator access");
    expect(admin.find(".segmented button", "Full")).toBeDefined();
    await admin.unmount();
    const device = await settings(false);
    expect(device.text()).not.toContain("Orchestrator access");
    expect(device.find(".segmented button", "Full")).toBeUndefined();
    await device.unmount();
  });
});

describe("what orchestrator.access says it did", () => {
  const result = (over: Json) => ({
    access: "full",
    previous: "limited",
    persisted: true,
    ...over,
  });
  it("says when the level applies", () => {
    expect(accessOutcome(result({ restart: "now" }))).toEqual({
      text: "The orchestrator restarted with full access. Its conversation continues.",
    });
    expect(accessOutcome(result({ restart: "after_turn" }))).toEqual({
      text: "The orchestrator is in a turn. It switches to full access when that turn ends.",
    });
    expect(accessOutcome(result({ restart: "none" }))).toEqual({
      text: "No orchestrator is running: the next one starts with full access.",
    });
    expect(
      accessOutcome(result({ restart: "none", previous: "full" })),
    ).toEqual({ text: "The orchestrator already has full access." });
  });
  it("warns when the change wasn't saved, with the hub's reason", () => {
    expect(
      accessOutcome(
        result({
          restart: "now",
          persisted: false,
          persist_error: "read-only file system",
        }),
      ),
    ).toEqual({
      text: "The orchestrator restarted with full access. Its conversation continues.",
      warning:
        "config.toml couldn't be updated (read-only file system), so this applies only until the hub restarts.",
    });
    expect(
      accessOutcome(result({ restart: "now", persisted: false })).warning,
    ).toBe(
      "config.toml couldn't be updated, so this applies only until the hub restarts.",
    );
  });
  it("reads only the levels the hub has", () => {
    expect(accessLevel("full")).toBe("full");
    expect(accessLevel("limited")).toBe("limited");
    expect(accessLevel(undefined)).toBeUndefined();
    expect(accessLevel("admin")).toBeUndefined();
  });
});

describe("the command palette's Orchestrator access", () => {
  const command = commands.find((c) => c.id === "access")!;
  it("calls orchestrator.access with the chosen level", () => {
    expect(command).toMatchObject({
      title: "Orchestrator access",
      group: "Orchestrator",
      method: "orchestrator.access",
    });
    expect(command.fields).toEqual([
      expect.objectContaining({
        key: "access",
        type: "select",
        required: true,
        options: ["limited", "full"],
      }),
    ]);
    expect(parameters(command, { access: "full" })).toEqual({ access: "full" });
  });
  it("asks before full access only", () => {
    expect(confirmationOf(command, { access: "full" })).toEqual(
      FULL_ACCESS_CONFIRM,
    );
    expect(confirmationOf(command, { access: "limited" })).toBeUndefined();
    // Commands with a fixed confirmation keep it.
    const clear = commands.find((c) => c.id === "clear")!;
    expect(confirmationOf(clear, {})).toEqual({
      title: clear.title,
      detail: "Start a fresh orchestrator conversation?",
    });
    expect(
      confirmationOf(
        commands.find((c) => c.id === "watch")!,
        {},
      ),
    ).toBeUndefined();
  });
});

describe("the orchestrator transcript's configuration lines", () => {
  const event = (data: Json, seq = 1): HubEvent =>
    ({
      id: "e" + seq,
      seq,
      thread: "orch",
      ts: "2026-11-18T10:00:00Z",
      kind: "config.changed",
      data,
    }) as HubEvent;
  it("says what changed, who changed it and when it applies, as the TUI does", () => {
    const blocks = blocksOf([
      event({
        key: "orchestrator.access",
        value: "full",
        before: "limited",
        by: "user",
        effect: "after_turn",
      }),
      event(
        { key: "tui.suggest", value: false, by: "orchestrator", effect: "now" },
        2,
      ),
      event(
        {
          key: "voice.pause_ms",
          value: 900,
          before: 700,
          by: "user",
          effect: "the next time a TUI starts",
        },
        3,
      ),
    ]);
    expect(blocks.map((b) => [b.kind, b.text])).toEqual([
      [
        "notice",
        `Set orchestrator.access = "full" (was "limited") (by you); takes effect when the orchestrator's turn ends`,
      ],
      ["notice", "Set tui.suggest = false (was unset); takes effect now"],
      [
        "notice",
        "Set voice.pause_ms = 900 (was 700) (by you); takes effect the next time a TUI starts",
      ],
    ]);
  });
});
