import { beforeAll, describe, expect, it } from "vitest";
import type { Json, Thread } from "../shared/types";
import {
  effortName,
  effortRows,
  modelRows,
  navigate,
  opensPicker,
  pickerRuntimes,
  planChange,
  runtimeMark,
  triggerText,
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

  it("switches the orchestrator with its summary, keeping its account on its runtime", () => {
    const orch = thread({
      id: "o",
      is_orchestrator: true,
      model: "opus",
      account: "work",
    });
    expect(
      planChange("orchestrator", orch, {
        runtime: "claude-code",
        model: "opus",
        effort: "max",
      }),
    ).toEqual({
      kind: "orchestrator",
      params: {
        runtime: "claude-code:work",
        model: "opus",
        effort: "max",
        summary: true,
      },
    });
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
    errors: unknown[];
    root: FakeElement;
    field: FakeElement;
    rerender: (props: Json) => Promise<void>;
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
        rpc: async (method: string, params: Json) => {
          calls.push({ method, params });
          if (props.refuse) throw new Error("refused");
          return {};
        },
        confirm: (c: Json) => confirms.push(c),
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
        errors,
        root: container,
        field,
        rerender: async (extra: Json) => {
          await act(async () =>
            root.render(createElement(ModelPicker, all(extra))),
          );
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
    expect(activeId(m.root)).toMatch(/efforts-3$/);
    expect(q(m.root, ".model-picker-efforts header")!.textContent).toContain(
      "sonnet",
    );
    expect(m.calls).toEqual([]);
    await m.act(() => void press(search(m.root) as any, "Enter"));
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
      await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
      await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
      expect(scrolled.at(-1)).toMatch(/rail-\d+$/);
      await m.act(() => void press(search(m.root) as any, "ArrowDown"));
      expect(scrolled.at(-1)).toBe(activeId(m.root));
      expect(activeId(m.root)).toMatch(/rail-\d+$/);
    } finally {
      proto.scrollIntoView = was;
    }
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

  it("asks before moving a thread to another runtime", async () => {
    const m = await mount({ target: thread({ model: "opus" }) });
    await m.act(() => click(q(m.root, ".model-picker-trigger")!));
    // ← on the empty search: the rail; ↓ codex, Enter: its models.
    await m.act(() => void press(search(m.root) as any, "ArrowLeft"));
    expect(activeId(m.root)).toMatch(/rail-0$/);
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(
      q(m.root, ".model-picker-models [role=listbox]")!.getAttribute(
        "aria-label",
      ),
    ).toBe("Models for codex");
    // Its default model, then effort low.
    await m.act(() => void press(search(m.root) as any, "Enter"));
    await m.act(() => void press(search(m.root) as any, "ArrowDown"));
    await m.act(() => void press(search(m.root) as any, "Enter"));
    expect(m.calls).toEqual([]);
    expect(m.confirms).toHaveLength(1);
    expect(m.confirms[0].title).toBe("Move api to codex, effort Low?");
    expect(m.confirms[0].detail).toContain("handoff");
    await m.act(() => m.confirms[0].run());
    expect(m.calls).toEqual([
      {
        method: "thread.set_model",
        params: { thread: "t1", runtime: "codex", effort: "low" },
      },
    ]);
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
        params: { runtime: "claude-code", effort: "max", summary: false },
      },
    ]);
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
    expect(activeId(m.root)).toMatch(/rail-0$/);
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
