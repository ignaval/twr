import { describe, expect, it, vi } from "vitest";
import type { Json, Thread } from "../shared/types";
import type { LoopState, RuntimeInfo } from "../shared/loops";
import { FakeEvent, installFakeDOM, type FakeElement } from "./fake-dom";

/* Loops on a remote box and ACP reviewers in the shared views (docs/design/loops.md, "Remote
 * boxes" and "ACP reviewers"), mounted on the stand-in DOM: a loop that waits on its box
 * says so in the inspector; with the box away nothing offers a start the hub would refuse,
 * and the choice that can't be made still takes the keyboard's focus and Escape; the
 * settings editor names, in words, each reviewer that can't be picked and what an ACP
 * reviewer's read-only rests on. */

const boxThread = (over: Partial<Thread> = {}): Thread =>
  ({
    id: "t1",
    name: "api",
    runtime: "fake",
    box_name: "devbox",
    box_online: false,
    mode: "auto",
    status: "done",
    accent: "#fff",
    is_orchestrator: false,
    project: "demo",
    ...over,
  }) as Thread;

const waiting: LoopState = {
  id: "l1",
  kind: "review",
  profile: "low",
  phase: "ladder",
  round: "m1",
  rounds: [{ id: "m1", findings: 0, new_class: 0, failed: true }],
  ledger: 0,
  verify: { done: 0, cap: 1 },
  paused: false,
  waiting_on: "review",
  box_away: "devbox",
  retried: true,
  reviewer: { runtime: "codex", model: "gpt-6.1-sol", effort: "medium" },
};

const runtimes: RuntimeInfo[] = [
  {
    runtime: "claude-code",
    found: true,
    login: true,
    models: ["claude-opus-5-5"],
    efforts: ["medium", "xhigh"],
  },
  {
    runtime: "codex",
    found: false,
    login: false,
    models: ["gpt-6.1-sol"],
    efforts: ["medium", "xhigh"],
  },
  {
    runtime: "ex",
    found: true,
    login: true,
    models: ["m-large", "m-small"],
    efforts: ["medium", "xhigh"],
    acp: true,
    review_mode: "read-only",
    cannot_review: null,
  },
  {
    runtime: "bare",
    found: true,
    login: true,
    models: [],
    efforts: [],
    acp: true,
    review_mode: null,
    cannot_review: "acp preset bare names no read-only mode",
  },
];

/** Mounts `element(views)` in the loops' context, the hub answering with `rpc`. */
async function mount(
  element: (views: Json, createElement: Json) => Json,
  rpc: (method: string, params?: Json) => Promise<Json>,
) {
  vi.resetModules();
  const document = installFakeDOM();
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const views = await import("../shared/LoopViews");
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const settle = () =>
    act(async () => {
      for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
    });
  await act(async () =>
    root.render(
      createElement(
        views.LoopContext.Provider,
        {
          value: {
            rpc,
            fail: () => {},
            access: { act: true },
            offerReview: true,
            startReview: async () => false,
            startImplement: async () => false,
            control: async () => false,
            openEditor: () => {},
            confirm: () => {},
            settingsVersion: 0,
            settingsChanged: () => {},
          },
        },
        element(views, createElement),
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
    unmount: () => act(async () => root.unmount()),
  };
}

describe("the inspector's Loops section on a box thread", () => {
  it("says a running loop waits on its box, and the round waits for it", async () => {
    const m = await mount(
      (v, h) =>
        h(v.LoopsSection, {
          thread: boxThread({ loop: waiting, loop_badge: "REVIEW LOOP" }),
          connected: true,
        }),
      async (method) => {
        if (method === "loop.get") return { id: "l1", rounds: [] };
        throw new Error(`unexpected ${method}`);
      },
    );
    expect(m.find(".loop-status")?.textContent).toBe(
      "REVIEW LOOP low · waiting on box devbox",
    );
    expect(m.find(".loop-rounds li")?.textContent).toBe(
      "m1 waits for box devbox",
    );
    // Its controls work while the box is away: the hub holds them.
    for (const name of ["Pause", "Stop…"])
      expect(m.find("button", name)?.getAttribute("disabled")).toBeNull();
    await m.unmount();
  });

  it("offers no start while the box is away, and says why in words", async () => {
    const m = await mount(
      (v, h) => h(v.LoopsSection, { thread: boxThread(), connected: true }),
      async (method) => {
        if (method === "loop.settings.get") throw new Error("not here");
        throw new Error(`unexpected ${method}`);
      },
    );
    const why = m.find(".loop-start-why");
    expect(why?.textContent).toBe(
      "Box devbox is away: a loop can start once it is back.",
    );
    for (const name of ["Review loop", "Implementation loop…"]) {
      const button = m.find(".loop-starts button", name)!;
      expect(button.getAttribute("disabled")).not.toBeNull();
      expect(button.getAttribute("aria-describedby")).toBe(
        why!.getAttribute("id"),
      );
    }
    await m.unmount();
    // The box back: both start, and nothing is said.
    const back = await mount(
      (v, h) =>
        h(v.LoopsSection, {
          thread: boxThread({ box_online: true }),
          connected: true,
        }),
      async () => {
        throw new Error("not here");
      },
    );
    expect(back.find(".loop-start-why")).toBeUndefined();
    expect(
      back.find(".loop-starts button", "Review loop")?.getAttribute("disabled"),
    ).toBeNull();
    await back.unmount();
  });
});

describe("Send as implementation loop that can't be made", () => {
  it("takes the focus to its panel, which says why, and Escape closes it", async () => {
    const m = await mount(
      (v, h) =>
        h(v.SendAsLoop, {
          thread: boxThread(),
          phone: false,
          busy: false,
          onSend: () => {},
        }),
      async () => ({}),
    );
    const opener = m.find('button[aria-label="More ways to send"]')!;
    await m.click(opener);
    const panel = m.find(".send-loop-panel")!;
    const choice = m.find("button", "Send as implementation loop")!;
    expect(choice.getAttribute("disabled")).not.toBeNull();
    expect(m.document.activeElement).toBe(panel);
    const said = m.find(`#${panel.getAttribute("aria-describedby")}`);
    expect(said?.textContent).toBe(
      "Box devbox is away: a loop can start once it is back.",
    );
    await m.key(panel, "Escape");
    expect(m.find(".send-loop-panel")).toBeUndefined();
    expect(m.document.activeElement).toBe(opener);
    await m.unmount();
  });
});

describe("the settings editor's reviewer picker", () => {
  it("names each reviewer that can't be picked, and what an ACP reviewer's read-only rests on", async () => {
    const settings = {
      profile: "low",
      implementer: null,
      workers: { model: "claude-opus-5-5", effort: "high" },
      reviewer: { runtime: "ex", model: "m-large" },
    };
    const m = await mount(
      (v, h) =>
        h(v.LoopSettingsForm, { source: { kind: "saved" }, inline: true }),
      async (method) => {
        if (method === "loop.settings.get")
          return {
            box: "devbox",
            settings,
            source: "workspace",
            first_card: settings,
            projects: [],
            presets: [],
            profiles: ["low", "medium", "high"],
            runtimes,
            warnings: [],
            problems: [],
          };
        throw new Error(`unexpected ${method}`);
      },
    );
    const skipped = m.find(".loop-editor-skipped");
    expect(skipped?.textContent).toBe(
      "Can't review on box devbox: codex (not installed); bare (acp preset bare names no read-only mode)",
    );
    const acp = m.find(".loop-editor-acp");
    expect(acp?.textContent).toBe(
      'ex is an ACP agent: it reviews in its own "read-only" mode, at the efforts medium, xhigh, which every round of the profile must use.',
    );
    // A screen reader hears both with the picker.
    const picker = m
      .find(".loop-editor-group", "Reviewer")!
      .querySelector("select")!;
    expect(picker.getAttribute("aria-describedby")?.split(" ")).toEqual([
      acp!.getAttribute("id"),
      skipped!.getAttribute("id"),
    ]);
    // No ACP preset is offered as the implementer.
    const implementer = m
      .find("label", "Implementer")!
      .querySelector("select")!;
    expect(
      implementer
        .querySelectorAll("option")
        .map((o: FakeElement) => o.getAttribute("value")),
    ).toEqual(["", "claude-code", "codex"]);
    await m.unmount();
  });
});

describe("a setup card shown in two places on a phone", () => {
  it("raised again after its editor saved, opens one sheet: the one it was opened from", async () => {
    vi.resetModules();
    const document = installFakeDOM();
    (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
    // A phone: the mobile app in a narrow window.
    document.documentElement.dataset.mobile = "true";
    (window as any).matchMedia = (query: string) => ({
      matches: true,
      media: query,
      addEventListener() {},
      removeEventListener() {},
      addListener() {},
      removeListener() {},
    });
    const { act, createElement, Fragment } = await import("react");
    const { createRoot } = await import("react-dom/client");
    const views = await import("../shared/LoopViews");
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container as unknown as Element);
    const item = (revision: number) =>
      ({
        attn_id: "a1",
        thread: "t1",
        revision,
        reason: "review",
        loop_setup: true,
        summary:
          "api: review loop setup (first run: choose its models). Start and save these settings?\nProfile      low",
        options: ["start and save these settings", "change…", "cancel"],
      }) as Json;
    const card = (revision: number) =>
      createElement(views.LoopSetupCard, {
        key: `a1@${revision}`,
        item: item(revision),
        readOnly: false,
        onRespond: async () => {},
      });
    // Needs you's list, and the thread's inspector, each with the card (keyed by revision,
    // as the app's lists key them).
    const render = (revision: number) =>
      act(async () =>
        root.render(
          createElement(
            Fragment,
            null,
            createElement("section", { key: "list" }, card(revision)),
            createElement(
              "aside",
              { key: "inspector", id: "twr-inspector" },
              card(revision),
            ),
          ),
        ),
      );
    const settle = () =>
      act(async () => {
        for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
      });
    const dialogs = () => document.querySelectorAll('[role="dialog"]').length;
    await render(1);
    await settle();
    expect(dialogs()).toBe(0);
    const opener = (
      document.querySelectorAll("#twr-inspector button") as FakeElement[]
    ).find((b) => b.textContent === "Review setup")!;
    await act(
      async () =>
        void opener.dispatchEvent(
          new FakeEvent("click", {
            bubbles: true,
            cancelable: true,
            button: 0,
          }),
        ),
    );
    await settle();
    expect(dialogs()).toBe(1);
    // The editor saved: the hub raises the card again at revision 2, and both copies
    // remount. One sheet stays open, the inspector's.
    await render(2);
    await settle();
    expect(dialogs()).toBe(1);
    const inspectorRow = document.querySelector(
      "#twr-inspector .loop-setup-row",
    );
    expect(
      inspectorRow?.querySelector("button")?.getAttribute("aria-expanded"),
    ).toBe("true");
    expect(
      document
        .querySelector("section .loop-setup-row button")
        ?.getAttribute("aria-expanded"),
    ).toBe("false");
    await act(async () => root.unmount());
  });
});
