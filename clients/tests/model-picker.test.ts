import { beforeAll, describe, expect, it } from "vitest";
import type { Json, Thread } from "../shared/types";
import {
  effortName,
  effortRows,
  modelRows,
  moveDetail,
  navigate,
  opensPicker,
  pickerRuntimes,
  planChange,
  runtimeMark,
  changeProblem,
  droppedBySwitch,
  droppedWords,
  notInstalled,
  switchDetail,
  triggerText,
  withoutDroppedText,
  type Nav,
} from "../shared/modelPick";
import {
  FakeElement as FakeElementClass,
  FakeEvent,
  installFakeDOM,
  press,
  typeInto,
  type FakeDocument,
  type FakeElement,
  type FakeInput,
} from "./fake-dom";

/* The composer's model and effort picker (shared/ModelPicker.tsx, shared/modelPick.ts):
 * what the trigger says, what each choice asks of the hub, and the popup driven by its
 * keyboard on the stand-in DOM. */

const catalog = {
  "claude-code": {
    models: [
      { id: "opus", description: "the latest Opus" },
      { id: "sonnet", description: "fast and capable" },
    ],
    efforts: ["low", "medium", "high", "xhigh", "max"],
  },
  codex: {
    models: [{ id: "gpt-5", description: "the default" }],
    efforts: ["low", "high"],
  },
  "grok-build": { models: [], efforts: ["low", "high"] },
};

const thread = (fields: Partial<Thread>): Thread => ({
  id: "t1",
  name: "api",
  runtime: "claude-code",
  box_name: "local",
  mode: "default",
  status: "idle",
  accent: "#3366ff",
  is_orchestrator: false,
  ...fields,
});

describe("the picker's rows and labels", () => {
  it("names the model the runtime reports, else the one asked, else Default", () => {
    expect(triggerText(thread({ model: "opus", effort: "high" }))).toEqual({
      model: "opus",
      effort: "High",
      label: "Model: opus · Effort: High. Change model",
    });
    expect(
      triggerText(thread({ model: "opus", model_actual: "claude-opus-5-5" }))
        .model,
    ).toBe("claude-opus-5-5");
    expect(triggerText(thread({})).label).toBe(
      "Model: Default · Effort: Default. Change model",
    );
    expect(effortName("xhigh")).toBe("Extra high");
    expect(runtimeMark("codex")).toBe("Cx");
    expect(runtimeMark("acp:gemini --acp")).toBe("Ge");
  });

  it("lists the catalog's runtimes, the current one first when it lists none", () => {
    expect(pickerRuntimes(catalog, "codex")).toEqual([
      "claude-code",
      "codex",
      "grok-build",
    ]);
    expect(pickerRuntimes(catalog, "pty")).toEqual([
      "pty",
      "claude-code",
      "codex",
      "grok-build",
    ]);
    expect(pickerRuntimes(undefined, "fake")).toEqual(["fake"]);
  });

  it("filters models by id or description, keeping a custom id and the one in effect", () => {
    expect(modelRows(catalog, "claude-code").map((r) => r.kind)).toEqual([
      "default",
      "model",
      "model",
      "custom",
    ]);
    expect(modelRows(catalog, "claude-code", "fast")).toEqual([
      { kind: "model", value: "sonnet", description: "fast and capable" },
      { kind: "custom" },
    ]);
    expect(modelRows(catalog, "claude-code", "", "my-model")).toContainEqual({
      kind: "model",
      value: "my-model",
      description: "set now",
    });
    expect(effortRows(catalog, "codex")).toEqual([null, "low", "high"]);
    expect(effortRows(catalog, "acp:x")).toEqual([null]);
  });
});

describe("what a choice asks of the hub", () => {
  const api = thread({ model: "opus", effort: "high" });

  it("sends only what changed on a thread's own runtime", () => {
    expect(
      planChange("thread", api, {
        runtime: "claude-code",
        model: "sonnet",
        effort: "high",
      }),
    ).toEqual({
      kind: "thread",
      params: { thread: "t1", model: "sonnet" },
      move: false,
    });
    // Back to the defaults: `default` clears each.
    expect(planChange("thread", api, { runtime: "claude-code" })).toEqual({
      kind: "thread",
      params: { thread: "t1", model: "default", effort: "default" },
      move: false,
    });
    expect(
      planChange("thread", api, {
        runtime: "claude-code",
        model: "opus",
        effort: "high",
      }),
    ).toEqual({ kind: "none" });
  });

  it("moves a thread to another runtime with what was picked", () => {
    expect(
      planChange("thread", api, { runtime: "codex", effort: "low" }),
    ).toEqual({
      kind: "thread",
      params: { thread: "t1", runtime: "codex", effort: "low" },
      move: true,
    });
  });

  it("sends a Default picked on a move, and leaves out a setting not picked", () => {
    // Back on a runtime the thread left a session of, one left out is that session's: a
    // Default picked must say `default`, or the hub restores the session's `high`.
    expect(
      planChange("thread", api, {
        runtime: "codex",
        chosen: { model: true, effort: true },
      }),
    ).toEqual({
      kind: "thread",
      params: {
        thread: "t1",
        runtime: "codex",
        model: "default",
        effort: "default",
      },
      move: true,
    });
    expect(
      planChange("thread", api, {
        runtime: "codex",
        model: "gpt-5",
        chosen: { model: true },
      }),
    ).toEqual({
      kind: "thread",
      params: { thread: "t1", runtime: "codex", model: "gpt-5" },
      move: true,
    });
    // On its own runtime and for the orchestrator nothing changes: `default` clears, and a
    // switch is always a new session.
    expect(
      planChange("thread", api, {
        runtime: "claude-code",
        model: "opus",
        chosen: { model: true, effort: true },
      }),
    ).toEqual({
      kind: "thread",
      params: { thread: "t1", effort: "default" },
      move: false,
    });
    expect(
      planChange("orchestrator", thread({ is_orchestrator: true }), {
        runtime: "codex",
        chosen: { model: true, effort: true },
      }),
    ).toEqual({
      kind: "orchestrator",
      params: { runtime: "codex", summary: true },
    });
  });

  it("says what a move starts: a new session, or the one the thread left there", () => {
    const untouched = moveDetail("api", { runtime: "codex" });
    expect(untouched).toBe(
      "A conversation cannot move between runtimes: the next message starts a new codex " +
        "session, with a handoff of what api did so far, on codex's default model and " +
        "effort. If api left a codex session earlier, that session resumes instead, told " +
        "what api did since, with the model and effort it had there.",
    );
    const reset = moveDetail("api", {
      runtime: "claude-code",
      model: "opus",
      chosen: { model: true, effort: true },
    });
    expect(reset).toContain("a new claude-code session, with a handoff");
    expect(reset).not.toContain("it had there");
    expect(reset).toContain(
      "Default picked for a setting claude-code keeps in a resumed session starts a new " +
        "session instead.",
    );
    // An ACP agent, a preset and a command line always start a new session.
    for (const runtime of ["acp:gemini --acp", "pty:bash", "myclaude"])
      expect(moveDetail("api", { runtime })).not.toContain("resumes");
    expect(moveDetail("api", { runtime: "pty:bash" })).not.toContain("default");
  });

  it("switches the orchestrator with its summary, the hub keeping its account on its runtime", () => {
    const orch = thread({
      id: "o",
      is_orchestrator: true,
      model: "opus",
      account: "work",
    });
    // The runtime alone, with `keep_account`: the hub keeps the account it recorded, never
    // one rebuilt from its name here.
    expect(
      planChange("orchestrator", orch, {
        runtime: "claude-code",
        model: "opus",
        effort: "max",
      }),
    ).toEqual({
      kind: "orchestrator",
      params: {
        runtime: "claude-code",
        model: "opus",
        effort: "max",
        summary: true,
        keep_account: true,
      },
    });
    // Another runtime: its own default account.
    expect(planChange("orchestrator", orch, { runtime: "codex" })).toEqual({
      kind: "orchestrator",
      params: { runtime: "codex", summary: true },
    });
    expect(
      planChange("orchestrator", orch, {
        runtime: "claude-code",
        model: "opus",
      }),
    ).toEqual({ kind: "none" });
  });

  it("keeps an ACP orchestrator's agent: a bare `acp`, no account after it", () => {
    const orch = thread({ id: "o", runtime: "acp", is_orchestrator: true });
    expect(
      planChange("orchestrator", orch, { runtime: "acp", model: "m2" }),
    ).toEqual({
      kind: "orchestrator",
      params: {
        runtime: "acp",
        model: "m2",
        summary: true,
        keep_account: true,
      },
    });
  });

  it("keeps the native login when only the effort changes, never naming it", () => {
    // On the native login the thread records no account. Its name (`default`) may be a
    // named account's too, which `codex:default` would get: nothing names it.
    const orch = thread({ id: "o", runtime: "codex", is_orchestrator: true });
    expect(
      planChange("orchestrator", orch, { runtime: "codex", effort: "high" }),
    ).toEqual({
      kind: "orchestrator",
      params: {
        runtime: "codex",
        effort: "high",
        summary: true,
        keep_account: true,
      },
    });
  });

  it("reads what went wrong with a change that went through", () => {
    expect(changeProblem({ thread: "o2" })).toBeUndefined();
    expect(changeProblem(undefined)).toBeUndefined();
    expect(
      changeProblem({
        warning: "grok is not installed: the new orchestrator cannot start",
        start_error: "spawn grok: not found",
      }),
    ).toBe(
      "grok is not installed: the new orchestrator cannot start. " +
        "The new orchestrator could not start: spawn grok: not found.",
    );
    expect(notInstalled({ installed: { codex: false } }, "codex")).toBe(true);
    expect(notInstalled({ installed: { codex: true } }, "codex")).toBe(false);
    // Not said (an older hub, a runtime twr doesn't know): not marked.
    expect(notInstalled({}, "codex")).toBe(false);
  });
});

describe("the popup's keys", () => {
  const sizes = { rail: 3, models: 4, efforts: 3 };
  const at: Nav = { column: "models", rail: 0, model: 1, effort: 0 };

  it("moves in a column, picks with Enter and closes with Esc", () => {
    expect(navigate(at, "ArrowDown", sizes, true)?.nav.model).toBe(2);
    expect(navigate(at, "ArrowUp", sizes, true)?.nav.model).toBe(0);
    expect(
      navigate({ ...at, model: 0 }, "ArrowUp", sizes, true)?.nav.model,
    ).toBe(3);
    expect(navigate(at, "Enter", sizes, true)?.effect).toBe("pick-model");
    expect(
      navigate({ ...at, column: "efforts" }, "Enter", sizes, true)?.effect,
    ).toBe("pick-effort");
    expect(
      navigate({ ...at, column: "rail" }, "Enter", sizes, true)?.effect,
    ).toBe("pick-runtime");
    expect(navigate(at, "Escape", sizes, true)?.effect).toBe("close");
    expect(navigate(at, "a", sizes, true)).toBeUndefined();
  });

  it("goes between columns with ← and →, ← to the rail only from an empty search", () => {
    expect(navigate(at, "ArrowRight", sizes, true)?.nav.column).toBe("efforts");
    expect(
      navigate({ ...at, column: "efforts" }, "ArrowLeft", sizes, true)?.nav
        .column,
    ).toBe("models");
    expect(navigate(at, "ArrowLeft", sizes, true)?.nav.column).toBe("rail");
    // Typing a search: the arrows move its caret.
    expect(navigate(at, "ArrowLeft", sizes, false)).toBeUndefined();
    expect(navigate(at, "ArrowRight", sizes, false, false)).toBeUndefined();
    expect(
      navigate({ ...at, column: "rail" }, "ArrowRight", sizes, true)?.nav
        .column,
    ).toBe("models");
  });

  it("opens with Ctrl+Shift+M, ⌘+Shift+M on a Mac", () => {
    const key = (init: Partial<Parameters<typeof opensPicker>[0]>) => ({
      key: "M",
      shiftKey: true,
      ctrlKey: false,
      metaKey: false,
      altKey: false,
      ...init,
    });
    expect(opensPicker(key({ ctrlKey: true }), false)).toBe(true);
    expect(opensPicker(key({ metaKey: true }), true)).toBe(true);
    expect(opensPicker(key({ ctrlKey: true }), true)).toBe(false);
    expect(opensPicker(key({ ctrlKey: true, shiftKey: false }), false)).toBe(
      false,
    );
  });
});

describe("the popup, rendered", () => {
  let document: FakeDocument;
  let mount: (props: Json) => Promise<{
    calls: { method: string; params: Json }[];
    confirms: Json[];
    withdrawn: Json[];
    errors: unknown[];
    root: FakeElement;
    field: FakeElement;
    rerender: (props: Json) => Promise<void>;
    unmount: () => Promise<void>;
    act: (f: () => unknown) => Promise<void>;
  }>;
  beforeAll(async () => {
    document = installFakeDOM() as FakeDocument;
    (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
    const { act, createElement } = await import("react");
    const { createRoot } = await import("react-dom/client");
    const { ModelPicker } = await import("../shared/ModelPicker");
    mount = async (props) => {
      const calls: { method: string; params: Json }[] = [];
      const confirms: Json[] = [];
      const withdrawn: Json[] = [];
      const errors: unknown[] = [];
      const container = document.createElement("div");
      document.body.appendChild(container);
      const field = document.createElement("textarea");
      document.body.appendChild(field);
      const root = createRoot(container as unknown as Element);
      const all = (extra: Json) => ({
        kind: "thread",
        catalog,
        connected: true,
        operate: true,
        admin: true,
        rpc: async (method: string, params: Json) => {
          // The read as the orchestrator's popup opens: answered apart, from `show`.
          if (method === "orchestrator.show") return props.show ?? {};
          calls.push({ method, params });
          if (props.refuse) throw new Error("refused");
          return props.reply ?? {};
        },
        confirm: (c: Json) => confirms.push(c),
        withdraw: (c: Json) => {
          withdrawn.push(c);
          return confirms.at(-1) === c;
        },
        fail: (e: unknown) => errors.push(e),
        input: { current: field },
        ...props,
        ...extra,
      });
      const settle = () =>
        act(async () => {
          for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r));
        });
      await act(async () => root.render(createElement(ModelPicker, all({}))));
      await settle();
      return {
        calls,
        confirms,
        withdrawn,
        errors,
        root: container,
        field,
        rerender: async (extra: Json) => {
          await act(async () =>
            root.render(createElement(ModelPicker, all(extra))),
          );
          await settle();
        },
        unmount: async () => {
          await act(async () => root.unmount());
          await settle();
        },
        act: async (f) => {
          await act(async () => void (await f()));
          await settle();
        },
      };
    };
  });
  const q = (root: FakeElement, selector: string) =>
    root.querySelector(selector) as FakeElement | null;
  const click = (node: FakeElement) =>
    node.dispatchEvent(
      new FakeEvent("click", { bubbles: true, cancelable: true, button: 0 }),
    );
  const search = (root: FakeElement) =>
    q(root, ".model-picker-search") as unknown as FakeInput;
  const activeId = (root: FakeElement) =>
    search(root).getAttribute("aria-activedescendant") || "";
  /** The focused element, and its id. */
  const focused = () => document.activeElement as unknown as FakeElement;
  const focusedId = () => focused()?.getAttribute("id") || "";

  it("is hidden without a destination, and disabled without the hub", async () => {
    const m = await mount({ target: undefined });
    expect(q(m.root, ".model-picker-trigger")).toBeNull();
    await m.rerender({ target: thread({}), connected: false });
    expect(q(m.root, ".model-picker-trigger")).not.toBeNull();
    const trigger = q(m.root, ".model-picker-trigger")!;
    expect(trigger.getAttribute("disabled")).not.toBeNull();
    expect(trigger.getAttribute("title")).toContain("Disconnected");
    await m.rerender({ target: thread({}), connected: true, operate: false });
    expect(trigger.getAttribute("title")).toContain("may not change models");
  });

  it("picks a thread's model, then its effort, and sends only what changed", async () => {
    const api = thread({ model: "opus", effort: "high" });
    const m = await mount({ target: api });
    const trigger = q(m.root, ".model-picker-trigger")!;
    expect(trigger.getAttribute("aria-label")).toBe(
      "Model: opus · Effort: High. Change model",
    );
    await m.act(() => click(trigger));
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
    expect(document.activeElement === (search(m.root) as any)).toBe(true);
    expect(activeId(m.root)).toMatch(/models-1$/);
    // ↓ to sonnet, Enter: the efforts, high (in effect) highlighted; the popup stays.
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
    // The focus on that effort (a screen reader says it), none in the models.
    expect(focusedId()).toMatch(/efforts-3$/);
    expect(activeId(m.root)).toBe("");
    expect(q(m.root, ".model-picker-efforts header")!.textContent).toContain(
      "sonnet",
    );
    expect(m.calls).toEqual([]);
    await m.act(() => void press(focused(), "Enter"));
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(m.calls).toEqual([
      { method: "thread.set_model", params: { thread: "t1", model: "sonnet" } },
    ]);
    expect(document.activeElement === m.field).toBe(true);
  });

  it("keeps the highlighted row in view as the keys move it, in every column", async () => {
    const scrolled: string[] = [];
    const proto = FakeElementClass.prototype as any;
    const was = proto.scrollIntoView;
    proto.scrollIntoView = function (this: FakeElement) {
      scrolled.push(this.getAttribute("id") || "");
    };
    try {
      const m = await mount({ target: thread({ model: "opus" }) });
      await m.act(() => click(q(m.root, ".model-picker-trigger")!));
      // It opens on the row in effect, scrolled to.
      expect(scrolled.at(-1)).toMatch(/models-1$/);
      await m.act(() => void press(search(m.root) as any, "ArrowDown"));
      expect(activeId(m.root)).toMatch(/models-2$/);
      expect(scrolled.at(-1)).toMatch(/models-2$/);
      await m.act(() => void press(search(m.root) as any, "ArrowRight"));
      expect(scrolled.at(-1)).toMatch(/efforts-\d+$/);
      await m.act(() => void press(focused(), "ArrowLeft"));
      await m.act(() => void press(focused(), "ArrowLeft"));
      expect(scrolled.at(-1)).toMatch(/rail-\d+$/);
      await m.act(() => void press(focused(), "ArrowDown"));
      expect(scrolled.at(-1)).toBe(focusedId());
      expect(focusedId()).toMatch(/rail-\d+$/);
    } finally {
      proto.scrollIntoView = was;
    }
  });

  it("fits the room above the composer, and opens over it when that is too little", async () => {
    // The composer (the popup's offset parent) under a column that clips at the top: a tall
    // draft leaves `top - 0` above it. The popup's height is that room, not the window's.
    const proto = FakeElementClass.prototype as any;
    const win = globalThis.window as any;
    const was = win.getComputedStyle;
    const rect = (top: number, bottom: number) => ({ top, bottom }) as DOMRect;
    let composer = rect(0, 0);
    const m = await mount({ target: thread({}) });
    Object.defineProperty(proto, "offsetParent", {
      configurable: true,
      get(this: FakeElement) {
        return this.classList.contains("model-picker-popup") ? m.root : null;
      },
    });
    const box = m.root.getBoundingClientRect;
    (m.root as any).getBoundingClientRect = () => composer;
    win.getComputedStyle = (n: FakeElement) =>
      n === (document.body as unknown as FakeElement)
        ? { overflowX: "clip", overflowY: "clip" }
        : { overflowX: "visible", overflowY: "visible" };
    const popup = () => q(m.root, ".model-picker-popup")!;
    const room = () => popup().style.getPropertyValue("--model-picker-room");
    try {
      composer = rect(606, 876);
      await m.act(() => click(q(m.root, ".model-picker-trigger")!));
      expect(room()).toBe(`${606 - 6 - 8}px`);
      expect(popup().dataset.over).toBeUndefined();
      await m.act(() => void press(search(m.root) as any, "Escape"));
      // 266 pixels above it: too few for its efforts and Done, so it opens over the
      // composer, from the column's top to the composer's bottom.
      composer = rect(266, 536);
      await m.act(() => click(q(m.root, ".model-picker-trigger")!));
      expect(popup().dataset.over).toBe("true");
      expect(room()).toBe(`${536 - 8 - 8}px`);
    } finally {
      delete proto.offsetParent;
      (m.root as any).getBoundingClientRect = box;
      win.getComputedStyle = was;
    }
    const { readFileSync } = await import("node:fs");
    const { join } = await import("node:path");
    const css = readFileSync(
      join(import.meta.dirname, "..", "shared", "model-picker.css"),
      "utf8",
    );
    expect(css).toContain(
      "max-height: min(420px, 60vh, var(--model-picker-room, 420px));",
    );
    const over = css.slice(
      css.indexOf('.model-picker-popup[data-over="true"] {'),
    );
    expect(over.slice(0, over.indexOf("}"))).toContain("bottom: 8px;");
  });

  it("searches, and closes with Esc, the focus back in the message box", async () => {
    const m = await mount({ target: thread({}) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => typeInto(search(m.root), "son"));
    const names = m.root
      .querySelectorAll(".model-picker-models [role=option]")
      .map((n: FakeElement) => n.textContent);
    expect(names).toEqual(["sonnetfast and capable", "Custom model id…"]);
    await m.act(() => void press(search(m.root) as any, "Escape"));
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(m.calls).toEqual([]);
    expect(document.activeElement === m.field).toBe(true);
  });

  it("closes when the focus moves outside it, as a click outside does", async () => {
    const m = await mount({ target: thread({}) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    // Within the popup (Tab to Done) it stays open.
    const doneButton = m.root
      .querySelectorAll(".model-picker-popup footer button")
      .at(-1) as FakeElement;
    await m.act(() => {
      doneButton.focus();
      doneButton.dispatchEvent(new FakeEvent("focusin", { bubbles: true }));
    });
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
    // Tab past Done, to a control behind it: closed, the focus left where it went.
    await m.act(() => {
      m.field.focus();
      m.field.dispatchEvent(new FakeEvent("focusin", { bubbles: true }));
    });
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(document.activeElement === m.field).toBe(true);
    expect(m.calls).toEqual([]);
  });

  it("asks before moving a thread to another runtime", async () => {
    const m = await mount({ target: thread({ model: "opus" }) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    // ← on the empty search: the rail; ↓ codex, Enter: its models.
    await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
    expect(focusedId()).toMatch(/rail-0$/);
    await m.act(() => void press(focused(), "ArrowDown"));
    await m.act(() => void press(focused(), "Enter"));
    expect(
      q(m.root, ".model-picker-models [role=listbox]")!.getAttribute(
        "aria-label",
      ),
    ).toBe("Models for codex");
    // Its default model, then effort low.
    await m.act(() => void press(focused(), "Enter"));
    await m.act(() => void press(focused(), "ArrowDown"));
    await m.act(() => void press(focused(), "Enter"));
    expect(m.calls).toEqual([]);
    expect(m.confirms).toHaveLength(1);
    // Default picked is sent: a move back would otherwise restore the session's model.
    expect(m.confirms[0].title).toBe(
      "Move api to codex, model Default, effort Low?",
    );
    expect(m.confirms[0].detail).toBe(
      moveDetail("api", {
        runtime: "codex",
        effort: "low",
        chosen: { model: true, effort: true },
      }),
    );
    await m.act(() => m.confirms[0].run());
    expect(m.calls).toEqual([
      {
        method: "thread.set_model",
        params: {
          thread: "t1",
          runtime: "codex",
          model: "default",
          effort: "low",
        },
      },
    ]);
  });

  it("moves back with Default picked as `default`, and leaves out what Done didn't pick", async () => {
    // On claude-code at high, moved to codex earlier; now back, Default effort picked.
    const api = thread({ runtime: "codex", model: "gpt-5", effort: "high" });
    const m = await mount({ target: api });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    const rail = () =>
      m.root.querySelectorAll(".model-picker-rail [role=option]");
    const claude = () =>
      rail().find((n: FakeElement) => n.textContent.startsWith("Cl"))!;
    await m.act(() => click(claude()));
    const effort = (name: string) =>
      m.root
        .querySelectorAll(".model-picker-efforts [role=radio]")
        .find((n: FakeElement) => n.textContent === name)!;
    await m.act(() => click(effort("Default")));
    expect(m.confirms[0].title).toBe(
      "Move api to claude-code, effort Default?",
    );
    expect(m.confirms[0].detail).toContain(
      "with the model it had there. Default picked for a setting",
    );
    await m.act(() => m.confirms[0].run());
    expect(m.calls.at(-1)).toEqual({
      method: "thread.set_model",
      params: { thread: "t1", runtime: "claude-code", effort: "default" },
    });
    // Done without picking either: both left out, the session's kept on a move back.
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => click(claude()));
    const done = m.root
      .querySelectorAll(".model-picker-efforts footer button")
      .find((n: FakeElement) => n.textContent === "Done")!;
    await m.act(() => click(done));
    expect(m.confirms[1].title).toBe("Move api to claude-code?");
    expect(m.confirms[1].detail).toContain(
      "with the model and effort it had there.",
    );
    await m.act(() => m.confirms[1].run());
    expect(m.calls.at(-1)).toEqual({
      method: "thread.set_model",
      params: { thread: "t1", runtime: "claude-code" },
    });
  });

  it("confirms an orchestrator switch, with or without the summary", async () => {
    const orch = thread({
      id: "o",
      name: "orchestrator",
      is_orchestrator: true,
    });
    const m = await mount({ target: orch, kind: "orchestrator" });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    // Default model, effort max (by a click).
    await m.act(() => void press(search(m.root) as any, "Enter"));
    const max = m.root
      .querySelectorAll(".model-picker-efforts [role=radio]")
      .find((n: FakeElement) => n.textContent === "Max")!;
    await m.act(() => click(max));
    expect(m.confirms).toHaveLength(1);
    const c = m.confirms[0];
    expect(c.title).toBe("Switch the orchestrator to claude-code, effort Max?");
    expect(c.action).toBe("Switch with summary");
    expect(c.alt.action).toBe("Switch without summary");
    await m.act(() => c.alt.run());
    expect(m.calls).toEqual([
      {
        method: "orchestrator.use",
        params: {
          runtime: "claude-code",
          effort: "max",
          summary: false,
          keep_account: true,
        },
      },
    ]);
    // An empty queue: nothing said about dropped messages.
    expect(c.detail).not.toContain("dropped");
  });

  it("says a switch drops the orchestrator's queued messages, and hands on what it dropped", async () => {
    const orch = thread({
      id: "o",
      name: "orchestrator",
      is_orchestrator: true,
    });
    const reply = {
      thread: "o2",
      previous: {
        dropped_prompts: 2,
        dropped_messages: [
          "look at this\nUploaded file: a.png\n/w/.twr-uploads/u1/a.png",
        ],
      },
    };
    const results: Json[] = [];
    const m = await mount({
      target: orch,
      kind: "orchestrator",
      queued: 2,
      reply,
      switched: (r: Json) => results.push(r),
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    const max = m.root
      .querySelectorAll(".model-picker-efforts [role=radio]")
      .find((n: FakeElement) => n.textContent === "Max")!;
    await m.act(() => click(max));
    const c = m.confirms[0];
    expect(c.detail).toContain(
      "2 queued messages to the orchestrator are dropped: without the summary, at once",
    );
    await m.act(() => c.alt.run());
    expect(results).toEqual([reply]);
    // With the summary, the same.
    await m.act(() => c.run());
    expect(results).toHaveLength(2);
  });

  /** What the screen reader was last told (the announcer speaks after 150 ms). */
  const spoken = async () => {
    await new Promise((r) => setTimeout(r, 200));
    return document.querySelector("[data-twr-live=polite]")?.textContent ?? "";
  };
  const codexOrchestrator = thread({
    id: "o1",
    name: "orchestrator",
    runtime: "codex",
    is_orchestrator: true,
  });
  const claudeOrchestrator = thread({
    id: "o2",
    name: "orchestrator",
    runtime: "claude-code",
    is_orchestrator: true,
  });

  it("closes, saying why, when another client switches the orchestrator underneath it", async () => {
    const m = await mount({ target: codexOrchestrator, kind: "orchestrator" });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
    await m.rerender({ target: claudeOrchestrator, kind: "orchestrator" });
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(await spoken()).toBe(
      "The orchestrator changed: the model picker closed. Open it again to pick.",
    );
    // Opened again, it starts from what runs now.
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    expect(
      q(m.root, ".model-picker-models [role=listbox]")!.getAttribute(
        "aria-label",
      ),
    ).toBe("Models for claude-code");
    expect(m.calls).toEqual([]);
  });

  it("closes when the thread's model changes elsewhere, or the thread is deleted", async () => {
    const m = await mount({ target: thread({ model: "opus" }) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.rerender({ target: thread({ model: "sonnet" }) });
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(await spoken()).toBe(
      "api changed: the model picker closed. Open it again to pick.",
    );
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.rerender({ target: undefined });
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(await spoken()).toBe(
      "api is gone: the model picker closed. Open it again to pick.",
    );
    // Progress the runtime reports (not what was asked) leaves an open popup as it is.
    await m.rerender({ target: thread({ model: "sonnet" }) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.rerender({
      target: thread({ model: "sonnet", model_actual: "sonnet-4" }),
    });
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
  });

  it("takes back a switch it asked to confirm when the orchestrator changes first", async () => {
    const m = await mount({ target: codexOrchestrator, kind: "orchestrator" });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    // Codex's default model, effort low: a switch, asked to confirm.
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.confirms).toHaveLength(1);
    const stale = m.confirms[0];
    await m.rerender({ target: claudeOrchestrator, kind: "orchestrator" });
    expect(m.withdrawn).toEqual([stale]);
    expect(await spoken()).toBe(
      "The orchestrator changed: the change you were confirming was cancelled.",
    );
    // A confirmation answered anyway (it was on screen as it changed) does nothing.
    await m.act(() => stale.run());
    await m.act(() => stale.alt.run());
    expect(m.calls).toEqual([]);
    expect(String(m.errors.at(-1))).toContain(
      "The orchestrator changed before you confirmed: pick again",
    );
  });

  it("drops a pick still reading the orchestrator when the picker goes away", async () => {
    // A slow `orchestrator.show`: the conversation is left (the picker unmounts) and another
    // workspace opens before it answers. Nothing is asked, so nothing reaches that one.
    let answer: (show: Json) => void = () => {};
    const show = new Promise<Json>((r) => (answer = r));
    const m = await mount({
      target: codexOrchestrator,
      kind: "orchestrator",
      show,
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.confirms).toEqual([]);
    await m.unmount();
    await m.act(() => answer({ thread: "o1" }));
    expect(m.confirms).toEqual([]);
    expect(m.calls).toEqual([]);
    expect(m.errors).toEqual([]);
  });

  it("takes back a switch it asked to confirm when the picker goes away", async () => {
    const m = await mount({ target: codexOrchestrator, kind: "orchestrator" });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.confirms).toHaveLength(1);
    const stale = m.confirms[0];
    await m.unmount();
    expect(m.withdrawn).toEqual([stale]);
    // Answered anyway (the app's dialog outlived the picker): another workspace may be
    // open by now, and the app's calls go to it, so nothing is sent.
    await m.act(() => stale.run());
    await m.act(() => stale.alt.run());
    expect(m.calls).toEqual([]);
    expect(String(m.errors.at(-1))).toContain(
      "The model picker closed before you confirmed: pick again",
    );
  });

  it("still switches once confirmed while nothing changed, and takes back nothing after", async () => {
    const m = await mount({ target: codexOrchestrator, kind: "orchestrator" });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => m.confirms[0].run());
    expect(m.calls).toEqual([
      {
        method: "orchestrator.use",
        params: {
          runtime: "codex",
          effort: "low",
          summary: true,
          keep_account: true,
        },
      },
    ]);
    // The switch it made gives the orchestrator a new thread: nothing to take back.
    await m.rerender({
      target: { ...codexOrchestrator, id: "o3", effort: "low" },
      kind: "orchestrator",
    });
    expect(m.withdrawn).toEqual([]);
  });

  it("keeps the native login when only the orchestrator's effort changes", async () => {
    // `work` is Codex's default account; this orchestrator runs on the native login, which
    // `orchestrator.show` names `default`, a named account's name too: the hub keeps it.
    const m = await mount({
      target: codexOrchestrator,
      kind: "orchestrator",
      show: { thread: "o1", account: "default", runtime: "codex" },
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    // Its default model, then effort Low.
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => m.confirms[0].alt.run());
    expect(m.calls).toEqual([
      {
        method: "orchestrator.use",
        params: {
          runtime: "codex",
          effort: "low",
          summary: false,
          keep_account: true,
        },
      },
    ]);
  });

  it("marks a runtime that isn't installed, and says so before switching to it", async () => {
    const m = await mount({
      target: codexOrchestrator,
      kind: "orchestrator",
      show: {
        thread: "o1",
        account: "default",
        installed: { "claude-code": true, codex: true, "grok-build": false },
      },
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    const rail = m.root.querySelectorAll(".model-picker-rail [role=option]");
    const grok = rail.find((n: FakeElement) =>
      n.textContent.includes("grok-build"),
    )!;
    expect(grok.textContent).toContain("not installed");
    expect(grok.getAttribute("title")).toBe(
      "grok-build: not installed on the hub's machine",
    );
    expect(
      rail.filter((n: FakeElement) => n.textContent.includes("not installed")),
    ).toHaveLength(1);
    await m.act(() => click(grok));
    // Its default model, then its default effort.
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.confirms[0].detail).toMatch(
      /^grok-build is not installed on the hub's machine: the new orchestrator can't start until it is\. /,
    );
    // A runtime that is there: nothing said.
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    const claude = m.root
      .querySelectorAll(".model-picker-rail [role=option]")
      .find((n: FakeElement) => n.textContent.includes("claude-code"))!;
    await m.act(() => click(claude));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.confirms[1].detail).not.toContain("not installed");
  });

  it("says what went wrong with a thread's move that went through", async () => {
    const notices: string[] = [];
    const m = await mount({
      target: thread({ model: "opus" }),
      reply: { thread: "t1", warning: "codex is not installed" },
      notify: (text: string) => notices.push(text),
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => m.confirms[0].run());
    expect(notices).toEqual(["codex is not installed."]);
  });

  it("shows a refusal through the app's error path", async () => {
    const m = await mount({ target: thread({}), refuse: true });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.calls).toHaveLength(1);
    expect(String(m.errors[0])).toContain("refused");
  });

  const option = (root: FakeElement, column: string, text: string) =>
    root
      .querySelectorAll(`${column} [role=option], ${column} [role=radio]`)
      .find((n: FakeElement) => n.textContent.startsWith(text))!;
  const customField = (root: FakeElement) =>
    q(root, ".model-picker-custom input") as unknown as FakeInput;
  const done = (root: FakeElement) =>
    q(root, ".model-picker-efforts footer .primary")!;
  const notice = (root: FakeElement) =>
    q(root, ".model-picker-notice")!.textContent;

  it("on a device without admin, changes a thread's model on its runtime but offers no move", async () => {
    const m = await mount({ target: thread({ model: "opus" }), admin: false });
    const trigger = q(m.root, ".model-picker-trigger")!;
    expect(trigger.getAttribute("disabled")).toBeNull();
    await m.act(() => click(trigger));
    // The other runtimes are listed, disabled, with the reason.
    const codex = option(m.root, ".model-picker-rail", "Cxcodex");
    expect(codex.getAttribute("aria-disabled")).toBe("true");
    expect(codex.getAttribute("title")).toContain(
      "needs a device with admin access",
    );
    const own = option(m.root, ".model-picker-rail", "Clclaude-code");
    expect(own.getAttribute("aria-disabled")).toBeNull();
    // A click, or Enter on it, is refused with the reason; the models stay the thread's.
    await m.act(() => click(codex));
    expect(notice(m.root)).toBe(
      "Moving api to codex needs a device with admin access.",
    );
    await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    const models = () =>
      q(m.root, ".model-picker-models [role=listbox]")!.getAttribute(
        "aria-label",
      );
    expect(models()).toBe("Models for claude-code");
    expect(m.confirms).toEqual([]);
    // Its own runtime's models and efforts still change.
    // → to the models, on opus; ↓ to sonnet.
    await m.act(() => void press(search(m.root) as any, "ArrowRight"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(notice(m.root)).toBe("");
    await m.act(() => click(option(m.root, ".model-picker-efforts", "Low")));
    expect(m.calls).toEqual([
      {
        method: "thread.set_model",
        params: { thread: "t1", model: "sonnet", effort: "low" },
      },
    ]);
    expect(m.errors).toEqual([]);
  });

  it("on a device without admin, a pty thread says why it can't move", async () => {
    const m = await mount({
      target: thread({ runtime: "pty", name: "sh" }),
      admin: false,
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    expect(q(m.root, ".model-picker-note")!.textContent).toContain(
      "Moving it to another runtime needs a device with admin access",
    );
    const focused = () => document.activeElement as FakeElement;
    await m.act(() => void press(focused(), "ArrowDown"));
    await m.act(() => void press(focused(), "Enter"));
    expect(search(m.root).getAttribute("disabled")).not.toBeNull();
    expect(notice(m.root)).toContain("needs a device with admin access");
    await m.act(() => click(done(m.root)));
    expect(m.calls).toEqual([]);
    expect(m.confirms).toEqual([]);
  });

  it("on a device without admin, the orchestrator's picker is disabled with the reason", async () => {
    const orch = thread({
      id: "o",
      name: "orchestrator",
      is_orchestrator: true,
    });
    const m = await mount({ target: orch, kind: "orchestrator", admin: false });
    const trigger = q(m.root, ".model-picker-trigger")!;
    expect(trigger.getAttribute("disabled")).not.toBeNull();
    expect(trigger.getAttribute("title")).toContain(
      "Switching the orchestrator needs a device with admin access",
    );
    await m.act(() => {
      window.dispatchEvent(
        new FakeEvent("keydown", {
          key: "M",
          ctrlKey: true,
          shiftKey: true,
          bubbles: true,
          cancelable: true,
        }) as unknown as Event,
      );
    });
    expect(q(m.root, "[role=dialog]")).toBeNull();
    // With admin (the local app has every scope), it opens.
    await m.rerender({ admin: true });
    expect(trigger.getAttribute("disabled")).toBeNull();
  });

  it("applies a typed custom model id with Done, and refuses an empty one", async () => {
    const m = await mount({
      target: thread({ model: "opus", effort: "high" }),
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() =>
      click(option(m.root, ".model-picker-models", "Custom model id")),
    );
    // Done with the field empty: refused, the popup open, the reason shown.
    await m.act(() => click(done(m.root)));
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
    expect(notice(m.root)).toBe(
      "Type a model id first, or pick a model from the list.",
    );
    expect(document.activeElement === (customField(m.root) as any)).toBe(true);
    await m.act(() => typeInto(customField(m.root), "opus-next"));
    await m.act(() => click(done(m.root)));
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(m.calls).toEqual([
      {
        method: "thread.set_model",
        params: { thread: "t1", model: "opus-next" },
      },
    ]);
  });

  it("applies a typed custom id with the effort picked, on another runtime too", async () => {
    const m = await mount({ target: thread({ model: "opus" }) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => click(option(m.root, ".model-picker-rail", "Cxcodex")));
    await m.act(() =>
      click(option(m.root, ".model-picker-models", "Custom model id")),
    );
    await m.act(() => typeInto(customField(m.root), "o9"));
    await m.act(() => click(option(m.root, ".model-picker-efforts", "High")));
    expect(m.confirms).toHaveLength(1);
    expect(m.confirms[0].title).toBe(
      "Move api to codex, model o9, effort High?",
    );
    await m.act(() => m.confirms[0].run());
    expect(m.calls).toEqual([
      {
        method: "thread.set_model",
        params: { thread: "t1", runtime: "codex", model: "o9", effort: "high" },
      },
    ]);
  });

  it("commits a custom id with its Use button, and shows it as the model chosen", async () => {
    const m = await mount({ target: thread({}) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() =>
      click(option(m.root, ".model-picker-models", "Custom model id")),
    );
    const use = q(m.root, ".model-picker-custom button")!;
    expect(use.textContent).toBe("Use");
    await m.act(() => click(use));
    expect(notice(m.root)).toContain("Type a model id first");
    await m.act(() => typeInto(customField(m.root), "mine"));
    await m.act(() => click(q(m.root, ".model-picker-custom button")!));
    // The field closes; its row shows the id, chosen, and the efforts are for it.
    expect(customField(m.root)).toBeNull();
    const row = option(m.root, ".model-picker-models", "Custom model id");
    expect(row.getAttribute("aria-selected")).toBe("true");
    expect(row.textContent).toContain("mine");
    expect(q(m.root, ".model-picker-efforts header")!.textContent).toContain(
      "mine",
    );
    expect(m.calls).toEqual([]);
    // Opened again, the field holds it.
    await m.act(() => click(row));
    expect(customField(m.root).value).toBe("mine");
    await m.act(() => click(done(m.root)));
    expect(m.calls).toEqual([
      { method: "thread.set_model", params: { thread: "t1", model: "mine" } },
    ]);
  });

  it("opens with Ctrl+Shift+M; a pty thread picks only another runtime", async () => {
    const m = await mount({ target: thread({ runtime: "pty", name: "sh" }) });
    await m.act(() => {
      window.dispatchEvent(
        new FakeEvent("keydown", {
          key: "M",
          ctrlKey: true,
          shiftKey: true,
          bubbles: true,
          cancelable: true,
        }) as unknown as Event,
      );
    });
    expect(q(m.root, "[role=dialog]")).not.toBeNull();
    expect(q(m.root, ".model-picker-note")!.textContent).toContain(
      "runs its own command line",
    );
    expect(q(m.root, ".model-picker-models [role=listbox]")).toBeNull();
    expect(focusedId()).toMatch(/rail-0$/);
    expect(activeId(m.root)).toBe("");
  });

  it("puts the focus on each runtime and effort the keys reach, a Tab stop in each column", async () => {
    const m = await mount({
      target: thread({ model: "opus", effort: "high" }),
    });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    const rows = (column: string) =>
      m.root.querySelectorAll(
        `.model-picker-${column} [role=option], .model-picker-${column} [role=radio]`,
      );
    const stops = (column: string) =>
      rows(column)
        .filter((n: FakeElement) => n.getAttribute("tabindex") === "0")
        .map((n: FakeElement) => n.getAttribute("id"));
    // The combobox's active descendant is always a model, in the listbox it controls.
    const listbox = search(m.root).getAttribute("aria-controls");
    const inModels = () =>
      q(m.root, `[id="${listbox}"] [id="${activeId(m.root)}"]`) !== null;
    expect(inModels()).toBe(true);
    // The highlighted runtime and effort are each column's one Tab stop; the models' is the
    // search.
    expect(stops("rail")).toEqual([rows("rail")[0].getAttribute("id")]);
    expect(stops("efforts")).toEqual([rows("efforts")[3].getAttribute("id")]);
    expect(stops("models")).toEqual([]);
    // → to the efforts: the focus on the effort in effect, nothing active in the models.
    await m.act(() => void press(search(m.root) as any, "ArrowRight"));
    expect(focusedId()).toBe(rows("efforts")[3].getAttribute("id"));
    expect(activeId(m.root)).toBe("");
    await m.act(() => void press(focused(), "ArrowDown"));
    expect(focusedId()).toBe(rows("efforts")[4].getAttribute("id"));
    expect(stops("efforts")).toEqual([focusedId()]);
    // ← back to the models: the search again, its active descendant a model.
    await m.act(() => void press(focused(), "ArrowLeft"));
    expect(focused() === (search(m.root) as any)).toBe(true);
    expect(inModels()).toBe(true);
    // ← to the rail: the focus on the runtime; ↓ moves it.
    await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
    expect(focusedId()).toBe(rows("rail")[0].getAttribute("id"));
    await m.act(() => void press(focused(), "ArrowDown"));
    expect(focusedId()).toBe(rows("rail")[1].getAttribute("id"));
    expect(activeId(m.root)).toBe("");
    // Typing there searches the models, from the search.
    await m.act(() => void press(focused(), "s"));
    expect(focused() === (search(m.root) as any)).toBe(true);
    expect((search(m.root) as any).value).toBe("s");
    expect(inModels()).toBe(true);
    // Tab to an effort (its focus, as the browser says it): the keys go on there; Esc
    // closes from it.
    const tabTo = (n: FakeElement) => {
      n.focus();
      n.dispatchEvent(new FakeEvent("focusin", { bubbles: true }));
    };
    await m.act(() => tabTo(rows("efforts")[1]));
    await m.act(() => void press(focused(), "ArrowDown"));
    expect(focusedId()).toBe(rows("efforts")[2].getAttribute("id"));
    // Shift+Tab back to the search: the keys move in the models again.
    await m.act(() => tabTo(search(m.root) as any));
    expect(inModels()).toBe(true);
    await m.act(() => tabTo(rows("efforts")[2]));
    await m.act(() => void press(focused(), "Escape"));
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(focused() === m.field).toBe(true);
    expect(m.calls).toEqual([]);
  });

  it("on a pty thread, puts the focus on the rail and lets the keys pick a runtime", async () => {
    const m = await mount({ target: thread({ runtime: "pty", name: "sh" }) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    const focused = () => document.activeElement as FakeElement | null;
    // The search is disabled: the focus is on the current runtime in the rail.
    expect(search(m.root).getAttribute("disabled")).not.toBeNull();
    expect(focused()?.getAttribute("id")).toMatch(/rail-0$/);
    expect(focused()?.getAttribute("role")).toBe("option");
    // → has no models to go to; ↓ moves the focus with the highlight.
    await m.act(() => void press(focused()!, "ArrowRight"));
    expect(focused()?.getAttribute("id")).toMatch(/rail-0$/);
    await m.act(() => void press(focused()!, "ArrowDown"));
    expect(focused()?.getAttribute("id")).toMatch(/rail-1$/);
    expect(focused()?.getAttribute("class")).toContain("active");
    // Esc closes it from any of its controls, Done too.
    const done = q(m.root, ".model-picker-efforts footer .primary")!;
    done.focus();
    await m.act(() => void press(done, "Escape"));
    expect(q(m.root, "[role=dialog]")).toBeNull();
    expect(focused() === m.field).toBe(true);
    // Enter on a runtime picks it: its models' search has the focus.
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    await m.act(() => void press(focused()!, "ArrowDown"));
    await m.act(() => void press(focused()!, "Enter"));
    expect(search(m.root).getAttribute("disabled")).toBeNull();
    expect(focused() === (search(m.root) as unknown as FakeElement)).toBe(true);
    expect(m.calls).toHaveLength(0);
  });
});

describe("the picker on a phone", () => {
  it("is a bottom sheet whose rows and trigger are 44-point targets", async () => {
    const { readFileSync } = await import("node:fs");
    const { join } = await import("node:path");
    const css = readFileSync(
      join(import.meta.dirname, "..", "shared", "model-picker.css"),
      "utf8",
    );
    const rule = (selector: string) => {
      const at = css.indexOf(selector + " {");
      expect(at, selector).toBeGreaterThan(-1);
      return css.slice(at, css.indexOf("}", at));
    };
    expect(rule('[data-mobile="true"] .model-picker-trigger')).toContain(
      "min-height: 44px",
    );
    const rows = rule(
      '[data-mobile="true"] .model-picker-popup button,\n[data-mobile="true"] .model-picker-search,\n[data-mobile="true"] .model-picker-custom > input',
    );
    expect(rows).toContain("min-height: 44px");
    expect(rows).toContain("min-width: 44px");
    const sheet = rule('[data-mobile="true"] .model-picker-popup');
    expect(sheet).toContain("position: fixed");
    expect(sheet).toContain("bottom: 0");
  });

  it("keeps each section its full height, which the sheet scrolls through", async () => {
    // Shrunk to the capped sheet, a section's rows (a long catalog, a short phone) ran over
    // the next section's and competed for its taps.
    const { readFileSync } = await import("node:fs");
    const { join } = await import("node:path");
    const css = readFileSync(
      join(import.meta.dirname, "..", "shared", "model-picker.css"),
      "utf8",
    );
    const selector =
      '[data-mobile="true"] .model-picker-rail,\n[data-mobile="true"] .model-picker-models,\n[data-mobile="true"] .model-picker-efforts {';
    const at = css.indexOf(selector);
    expect(at).toBeGreaterThan(-1);
    const sections = css.slice(at, css.indexOf("}", at));
    expect(sections).toContain("flex-shrink: 0");
    expect(sections).toContain("overflow: visible");
    const sheet = css.slice(
      css.indexOf('[data-mobile="true"] .model-picker-popup {'),
    );
    expect(sheet.slice(0, sheet.indexOf("}"))).toContain("overflow: auto");
  });
});

describe("the picker in a narrow window", () => {
  it("fits its columns in the composer and lets each scroll inside the popup", async () => {
    const { readFileSync } = await import("node:fs");
    const { join } = await import("node:path");
    const css = readFileSync(
      join(import.meta.dirname, "..", "shared", "model-picker.css"),
      "utf8",
    );
    const at = css.indexOf(".model-picker-popup {");
    const popup = css.slice(at, css.indexOf("}", at));
    // The composer's width, not the window's: beside a sidebar the window's ran the popup
    // past the conversation column, which clips it.
    expect(popup).toContain("width: min(640px, calc(100% - 16px))");
    expect(popup).toContain("right: 8px");
    // One row, the popup's height at most: an auto row grew with a long model list, the
    // popup clipped the columns, and Done went out of reach.
    expect(popup).toContain("grid-template-rows: minmax(0, 1fr)");
    // The columns' least widths fit the composer of the smallest window (420 px; the
    // composer is 32 px narrower there, the popup 16 px narrower than that, less its border).
    const columns = /grid-template-columns:([^;]+);/.exec(popup)![1];
    const least = [...columns.matchAll(/minmax\(\s*(\d+)px/g)].map((m) =>
      Number(m[1]),
    );
    expect(least).toHaveLength(3);
    expect(least.reduce((a, b) => a + b, 0)).toBeLessThanOrEqual(
      420 - 32 - 16 - 2,
    );
  });
});

describe("the popup without a catalog", () => {
  it("applies a model at once and keeps the thread's effort", async () => {
    // An older hub lists no models: the default and a custom id, and no efforts.
    const rows = modelRows(undefined, "claude-code");
    expect(rows.map((r) => r.kind)).toEqual(["default", "custom"]);
    expect(effortRows(undefined, "claude-code")).toEqual([null]);
    const api = thread({ model: "opus", effort: "high" });
    expect(
      planChange("thread", api, {
        runtime: "claude-code",
        model: "sonnet",
        effort: "high",
      }),
    ).toEqual({
      kind: "thread",
      params: { thread: "t1", model: "sonnet" },
      move: false,
    });
  });
});

describe("what an orchestrator switch drops", () => {
  it("says so before, in the confirmation", () => {
    expect(switchDetail(0)).not.toContain("dropped");
    expect(switchDetail(1)).toContain(
      "1 queued message to the orchestrator is dropped",
    );
  });

  it("reads the switch's result, and offers to put the user's own back", () => {
    expect(droppedBySwitch({ previous: null })).toEqual({
      count: 0,
      mine: [],
      lost: 0,
    });
    expect(droppedWords(0, 0)).toBeUndefined();
    const r = droppedBySwitch({
      previous: {
        dropped_prompts: 3,
        dropped_messages: ["one", " ", 7, "two"],
      },
    });
    expect(r).toEqual({ count: 3, mine: ["one", "two"], lost: 0 });
    expect(droppedWords(r.count, r.mine.length)).toEqual({
      title: "Put your 2 queued messages back in the message box?",
      detail:
        "The switch dropped 3 queued messages the previous orchestrator hadn't started. " +
        "2 of them were yours: put back, with their files, you can send them to the new " +
        "orchestrator.",
    });
    // Those the hub left out, too long for its answer, are said to be lost.
    const big = droppedBySwitch({
      previous: {
        dropped_prompts: 5,
        dropped_messages: ["one"],
        dropped_messages_omitted: 3,
      },
    });
    expect(big).toEqual({ count: 5, mine: ["one"], lost: 3 });
    expect(
      (droppedWords(big.count, big.mine.length, big.lost) as Json).detail,
    ).toBe(
      "The switch dropped 5 queued messages the previous orchestrator hadn't started. " +
        "1 of them was yours: put back, with their files, you can send them to the new " +
        "orchestrator. 3 more of yours were too long to return and can't be put back.",
    );
    expect(droppedWords(2, 0, 2)).toEqual({
      notice:
        "The switch dropped 2 queued messages the previous orchestrator hadn't started. " +
        "2 of them were yours, too long to return: they can't be put back.",
    });
    expect(droppedWords(1, 1)).toMatchObject({
      title: "Put your queued message back in the message box?",
    });
    expect((droppedWords(1, 1) as Json).detail).toContain("It was yours:");
    // Shown without their text.
    expect(
      withoutDroppedText({
        thread: "o2",
        previous: { dropped_prompts: 1, dropped_messages: ["x"] },
      }),
    ).toEqual({ thread: "o2", previous: { dropped_prompts: 1 } });
    expect(withoutDroppedText({ previous: null })).toEqual({ previous: null });
    // None of them the user's: a notice only.
    expect(droppedWords(2, 0)).toEqual({
      notice:
        "The switch dropped 2 queued messages the previous orchestrator hadn't started.",
    });
  });
});
