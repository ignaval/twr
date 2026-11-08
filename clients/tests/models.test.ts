import { describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  baseRuntime,
  effortChoices,
  formRuntime,
  modelChoices,
} from "../shared/models";
import { commands, parameters } from "../shared/commands";
import { ModelSelect } from "../shared/ui";
import type { Thread } from "../shared/types";

vi.mock("../shared/host", () => ({ useHostAPI: () => ({}) }));

// As `orchestrator.show` `models` reports it.
const catalog = {
  "claude-code": {
    models: [
      { id: "opus", description: "the latest Opus" },
      { id: "claude-opus-5-5", description: "Opus 5.5" },
    ],
    efforts: ["low", "medium", "high", "xhigh", "max"],
  },
  codex: {
    models: [{ id: "gpt-6.1-sol", description: "workhorse" }],
    efforts: ["low", "high"],
  },
};
const thread = (extra: Partial<Thread>): Thread => ({
  id: "01T",
  name: "api",
  runtime: "codex",
  box_name: "local",
  mode: "auto",
  status: "idle",
  accent: "",
  is_orchestrator: false,
  ...extra,
});

describe("model choices", () => {
  it("lists the catalog's models and efforts for a runtime, account part ignored", () => {
    expect(modelChoices(catalog, "claude-code:work")).toEqual([
      { value: "opus", description: "the latest Opus" },
      { value: "claude-opus-5-5", description: "Opus 5.5" },
    ]);
    expect(effortChoices(catalog, "codex").map((c) => c.value)).toEqual([
      "low",
      "high",
    ]);
    expect(baseRuntime("acp:my agent")).toBe("acp:my agent");
    // An unknown runtime, or an older hub with no catalog: free text.
    expect(modelChoices(catalog, "acp:my agent")).toEqual([]);
    expect(modelChoices(undefined, "claude-code")).toEqual([]);
    expect(effortChoices(null, "codex")).toEqual([]);
  });

  it("finds the runtime a form is about", () => {
    const threads = [
      thread({}),
      thread({
        id: "01O",
        name: "orchestrator",
        runtime: "claude-code",
        is_orchestrator: true,
      }),
    ];
    const model = { id: "model" };
    expect(formRuntime(model, { thread: "api" }, threads)).toBe("codex");
    expect(formRuntime(model, { thread: "01T" }, threads)).toBe("codex");
    // A move names the runtime: its models.
    expect(
      formRuntime(model, { thread: "api", runtime: "grok-build" }, threads),
    ).toBe("grok-build");
    expect(formRuntime({ id: "orch" }, {}, threads)).toBe("claude-code");
    expect(formRuntime({ id: "new" }, {}, threads, "codex")).toBe("codex");
    expect(formRuntime({ id: "new" }, {}, [])).toBeUndefined();
  });

  it("sends a custom id trimmed, and an unset model not at all", () => {
    const orch = commands.find((c) => c.id === "orch")!;
    expect(orch.fields.find((f) => f.key === "model")?.type).toBe("model");
    expect(
      parameters(orch, { runtime: "claude-code", model: "  claude-x-1 " }),
    ).toMatchObject({ runtime: "claude-code", model: "claude-x-1" });
    expect(
      parameters(orch, { runtime: "claude-code", model: "" }),
    ).not.toHaveProperty("model");
    const set = commands.find((c) => c.id === "model")!;
    expect(set.fields.find((f) => f.key === "model")?.keep).toBe(true);
    expect(set.fields.find((f) => f.key === "effort")?.type).toBe("effort");
  });
});

describe("the model picker", () => {
  const render = (props: Parameters<typeof ModelSelect>[0]) =>
    renderToStaticMarkup(createElement(ModelSelect, props));
  const choices = modelChoices(catalog, "claude-code");

  it("offers the runtime's default first, the catalog's models, then a custom id", () => {
    const html = render({ value: "", onChange: () => {}, choices });
    expect(html).toContain("<select");
    const options = [...html.matchAll(/<option[^>]*>([^<]*)<\/option>/g)].map(
      (m) => m[1],
    );
    expect(options).toEqual([
      "Runtime default",
      "opus — the latest Opus",
      "claude-opus-5-5 — Opus 5.5",
      "Custom model id…",
    ]);
  });

  it("keeps the current one when asked to, with the default apart", () => {
    const html = render({ value: "", onChange: () => {}, choices, keep: true });
    expect(html).toContain(">Keep current</option>");
    expect(html).toContain('value="default"');
  });

  it("shows an id the list doesn't have as a custom one, in a text box", () => {
    const html = render({ value: "claude-x-9", onChange: () => {}, choices });
    expect(html).not.toContain("<select");
    expect(html).toContain('value="claude-x-9"');
    expect(html).toContain('aria-label="Choose from the list"');
  });

  it("offers no custom value for an effort", () => {
    const html = render({
      value: "",
      onChange: () => {},
      choices: effortChoices(catalog, "codex"),
      custom: false,
    });
    expect(html).not.toContain("Custom model id");
    expect(html).toContain(">high</option>");
  });
});
