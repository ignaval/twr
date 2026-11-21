import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { commands, parameters, type Command } from "../shared/commands";
import {
  accountChoices,
  boxChoices,
  deviceChoices,
  heldChoices,
  jobChoices,
  memoryChoices,
  pickerSources,
  resolveField,
  runtimeChoices,
  splitFields,
  threadChoices,
  trashChoices,
  watchChoices,
  type PickerContext,
} from "../shared/fieldChoices";
import { FieldInput } from "../shared/ui";
import type { Thread } from "../shared/types";
import { FakeEvent, installFakeDOM, type FakeElement } from "./fake-dom";

(globalThis as any).window ??= { twr: {} };

const thread = (extra: Partial<Thread>): Thread =>
  ({
    id: "t1",
    name: "api",
    runtime: "codex",
    box_name: "local",
    mode: "auto",
    status: "running",
    accent: "",
    is_orchestrator: false,
    ...extra,
  }) as Thread;
const threads = [
  thread({ id: "o", name: "orchestrator", is_orchestrator: true }),
  thread({ id: "t1", name: "api", aliases: ["backend"] }),
  thread({ id: "t2", name: "web", status: "waiting", runtime: "claude-code" }),
];
const command = (id: string) => commands.find((c) => c.id === id)!;
const field = (c: Command, key: string) => c.fields.find((f) => f.key === key)!;
const context = (
  c: Command,
  extra: Partial<PickerContext> = {},
): PickerContext => ({
  command: c,
  values: {},
  threads,
  admin: true,
  lists: {},
  ...extra,
});
const options = (html: string) =>
  [...html.matchAll(/<option[^>]*value="([^"]*)"[^>]*>([^<]*)<\/option>/g)].map(
    (m) => [m[1], m[2]],
  );

describe("command form pickers", () => {
  it("lists threads by id with name and status, the orchestrator last", () => {
    expect(threadChoices(threads)).toEqual([
      { value: "t1", label: "api · running" },
      { value: "t2", label: "web · waiting" },
      { value: "o", label: "Orchestrator · running" },
    ]);
  });

  it("keeps a thread named by name or alias on its entry, and shows an unknown one", () => {
    const rename = command("rename");
    const byAlias = resolveField(
      field(rename, "thread"),
      context(rename, { values: { thread: "backend" } }),
    );
    expect(byAlias.choices?.map((c) => c.value)).toEqual(["t1", "t2", "o"]);
    const unknown = resolveField(
      field(rename, "thread"),
      context(rename, { values: { thread: "gone" } }),
    );
    expect(unknown.choices?.at(-1)).toEqual({ value: "gone", label: "gone" });
  });

  it("offers installed runtimes, the built-ins first, and takes another under Other…", () => {
    expect(
      runtimeChoices({ agents: [{ runtime: "codex" }, { runtime: "pi" }] }, [
        thread({ runtime: "acp:gemini" }),
      ]).map((c) => c.value),
    ).toEqual(["claude-code", "codex", "grok-build", "pi", "acp:gemini"]);
    const set = command("model");
    const runtime = resolveField(
      field(set, "runtime"),
      context(set, { values: { runtime: "acp:my-agent" } }),
    );
    expect(runtime.keep).toBe(true);
    expect(runtime.custom).toBe(true);
    expect(runtime.choices?.map((c) => c.value)).not.toContain("acp:my-agent");
    const html = renderToStaticMarkup(
      createElement(FieldInput, { field: runtime, value: "", onChange() {} }),
    );
    expect(options(html)[0]).toEqual(["", "Keep current"]);
    // A one-off acp: runtime: Other…, last, turns the list into a text box.
    expect(options(html).at(-1)?.[1]).toBe("Other…");
    // A value the list lacks shows typed, with a way back to the list.
    const typed = renderToStaticMarkup(
      createElement(FieldInput, {
        field: runtime,
        value: "acp:my-agent",
        onChange() {},
      }),
    );
    expect(typed).not.toContain("<select");
    expect(typed).toMatch(
      /<input[^>]*id="field-runtime"[^>]*aria-label="Runtime"[^>]*value="acp:my-agent"/,
    );
    expect(typed).toContain('aria-label="Choose runtime from the list"');
    // The New thread form's runtime takes one too; the CLI catalog's stays text.
    expect(
      resolveField(field(command("new"), "runtime"), context(command("new")))
        .custom,
    ).toBe(true);
  });

  it("takes an account pool or pattern under Other…", () => {
    const list = {
      runtimes: {
        codex: { accounts: [{ name: "work-a" }, { name: "work-b" }] },
      },
    };
    for (const id of ["new", "model"]) {
      const c = command(id);
      const account = resolveField(
        field(c, "account"),
        context(c, {
          values: { thread: "t1", runtime: "codex", account: "work-*" },
          lists: { "account.list": list },
        }),
      );
      expect(account.custom).toBe(true);
      expect(account.placeholder).toContain("work-a,work-b");
      expect(account.choices?.map((c) => c.value)).toEqual([
        "work-a",
        "work-b",
      ]);
      const html = renderToStaticMarkup(
        createElement(FieldInput, {
          field: account,
          value: "work-*",
          onChange() {},
        }),
      );
      expect(html).toMatch(/<input[^>]*value="work-\*"/);
    }
    // While its list loads it is a disabled list showing the value; a failed one is text.
    const loading = resolveField(
      field(command("model"), "account"),
      context(command("model"), { values: { account: "work-a,work-b" } }),
    );
    expect(
      renderToStaticMarkup(
        createElement(FieldInput, {
          field: loading,
          value: "work-a,work-b",
          onChange() {},
        }),
      ),
    ).toMatch(/<select[^>]*disabled/);
  });

  it("puts the local box first", () => {
    expect(
      boxChoices({
        boxes: [
          { name: "gpu", online: true },
          { name: "mini", online: false },
        ],
      }),
    ).toEqual([
      { value: "local", label: "local" },
      { value: "gpu", label: "gpu" },
      { value: "mini", label: "mini · offline" },
    ]);
  });

  it("offers the chosen runtime's accounts, or the thread's runtime's", () => {
    const list = {
      runtimes: {
        codex: { accounts: [{ name: "work", login: "logged_in" }] },
        "claude-code": {
          accounts: [
            { name: "personal", native: true },
            { name: "old", removed: true },
          ],
        },
      },
    };
    expect(accountChoices(list, "codex").map((c) => c.value)).toEqual(["work"]);
    expect(accountChoices(list).map((c) => c.label)).toEqual([
      "work · codex",
      "personal · claude-code",
    ]);
    const set = command("model");
    const account = resolveField(
      field(set, "account"),
      context(set, {
        values: { thread: "t2" },
        lists: { "account.list": list },
      }),
    );
    expect(account.choices?.map((c) => c.value)).toEqual(["personal"]);
  });

  it("draws a loading list as a disabled select, and a failed one as a text box", () => {
    const cancel = command("job-cancel");
    const loading = resolveField(field(cancel, "job"), context(cancel));
    expect(loading.loading).toBe(true);
    expect(
      renderToStaticMarkup(
        createElement(FieldInput, { field: loading, value: "", onChange() {} }),
      ),
    ).toMatch(/<select[^>]*disabled/);
    const failed = resolveField(
      field(cancel, "job"),
      context(cancel, { lists: { "job.list": null } }),
    );
    expect(failed.type).toBe("text");
    expect(pickerSources(cancel)).toEqual(["job.list"]);
  });

  it("names jobs, trash, watches, devices, notes and held messages", () => {
    expect(
      jobChoices([
        { job: "J1", name: "test", thread_name: "api", state: "running" },
      ]),
    ).toEqual([{ value: "J1", label: "test · api · running" }]);
    expect(
      trashChoices({
        items: [
          { item: "a", thread_name: "old", deleted_at: "" },
          { item: "b", incomplete: true },
        ],
      }).map((c) => c.value),
    ).toEqual(["a"]);
    expect(
      watchChoices({
        watches: [{ watch: "w1", thread_name: "api", note: "tell me" }],
      }),
    ).toEqual([
      { value: "w1", label: "api · tell me" },
      { value: "all", label: "Every watch" },
    ]);
    expect(watchChoices({ watches: [] })).toEqual([]);
    expect(
      deviceChoices([{ id: "d1", name: "Pixel", scopes: ["read", "operate"] }]),
    ).toEqual([{ value: "d1", label: "Pixel · Read and operate" }]);
    expect(memoryChoices({ notes: [{ name: "style" }] })).toEqual([
      { value: "style", label: "style" },
    ]);
    expect(
      heldChoices([
        thread({
          name: "web",
          queued_after: [{ id: "H1", after: ["api"], prompt: "deploy it" }],
        }),
      ]),
    ).toEqual([{ value: "H1", label: "To web · after api · deploy it" }]);
  });

  it("labels permission modes, narrowed without admin", () => {
    const mode = command("mode");
    const admin = resolveField(field(mode, "mode"), context(mode));
    expect(admin.choices?.find((c) => c.value === "auto")?.label).toBe(
      "Agent decides",
    );
    const narrow = resolveField(
      field(mode, "mode"),
      context(mode, { admin: false }),
    );
    expect(narrow.choices?.map((c) => c.value)).toEqual([
      "plan",
      "read-only",
      "dontAsk",
      "manual",
    ]);
    expect(narrow.custom).toBe(false);
    // New thread's mode is the same labelled picker.
    expect(field(command("new"), "mode").type).toBe("permission-mode");
  });

  it("offers an admin the runtime's own modes, and any other id under Other…", () => {
    const mode = command("mode");
    // api runs on Codex: its own ids follow twr's.
    const codex = resolveField(
      field(mode, "mode"),
      context(mode, { values: { thread: "t1", mode: "auto" } }),
    );
    expect(codex.custom).toBe(true);
    expect(codex.choices?.map((c) => c.value).slice(-5)).toEqual([
      "untrusted",
      "on-request",
      "never",
      "danger-full-access",
      "yolo",
    ]);
    expect(codex.choices?.find((c) => c.value === "on-request")?.label).toBe(
      "Codex: on-request",
    );
    // web runs on Claude Code: twr's modes only, and a mode its list lacks shows typed.
    const claude = resolveField(
      field(mode, "mode"),
      context(mode, { values: { thread: "t2", mode: "agent" } }),
    );
    expect(claude.choices?.map((c) => c.value)).not.toContain("on-request");
    expect(
      renderToStaticMarkup(
        createElement(FieldInput, {
          field: claude,
          value: "agent",
          onChange() {},
        }),
      ),
    ).toMatch(/<input[^>]*id="field-mode"[^>]*value="agent"/);
    // New thread's mode follows the runtime chosen in the form.
    const create = command("new");
    expect(
      resolveField(
        field(create, "mode"),
        context(create, { values: { runtime: "codex" } }),
      ).choices?.some((c) => c.value === "never"),
    ).toBe(true);
    // Without admin: the narrow list and no Other….
    const narrow = resolveField(
      field(mode, "mode"),
      context(mode, { admin: false, values: { thread: "t1", mode: "auto" } }),
    );
    expect(narrow.choices?.map((c) => c.value)).toEqual([
      "plan",
      "read-only",
      "dontAsk",
      "manual",
      "auto",
    ]);
    expect(
      renderToStaticMarkup(
        createElement(FieldInput, {
          field: narrow,
          value: "auto",
          onChange() {},
        }),
      ),
    ).not.toContain("Other…");
  });

  it("sends pairing access as the scopes list and usage periods the bridge reads", () => {
    const pair = command("device-pair");
    expect(parameters(pair, { scopes: "read operate terminal" })).toEqual({
      scopes: ["read", "operate", "terminal"],
    });
    const usage = command("usage-threads");
    expect(field(usage, "since").choices?.map((c) => c.value)).toEqual([
      "1d",
      "7d",
      "30d",
    ]);
    expect(parameters(usage, { since: "", by: "thread" })).toEqual({
      by: "thread",
    });
  });

  it("keeps the CLI catalog's runtime a text box with suggestions", () => {
    const cli: Command = {
      id: "cli:account login",
      title: "twr account login",
      description: "",
      group: "CLI",
      cli: ["account", "login"],
      fields: [{ key: "runtime", label: "runtime", type: "text" }],
    };
    const resolved = resolveField(cli.fields[0], context(cli));
    expect(resolved.type).toBe("text");
    expect(resolved.options).toContain("claude-code");
  });
});

describe("More options", () => {
  const create = command("new");

  it("keeps Project, Name, First prompt and the loop switch out of it", () => {
    const { essential, advanced, open } = splitFields(create.fields, {});
    expect(essential.map((f) => f.key)).toEqual([
      "project",
      "name",
      "prompt",
      "implement",
    ]);
    expect(advanced.map((f) => f.key)).toEqual([
      "loop_setup",
      "runtime",
      "box",
      "model",
      "effort",
      "mode",
      "account",
      "aliases",
      "base",
      "workspace_mode",
      "outside_pin",
    ]);
    expect(open).toBe(false);
  });

  it("opens when an advanced field holds a value, or the error names one", () => {
    expect(splitFields(create.fields, { runtime: "fake" }).open).toBe(true);
    expect(splitFields(create.fields, { workspace_mode: "default" }).open).toBe(
      false,
    );
    expect(splitFields(create.fields, { outside_pin: true }).open).toBe(true);
    expect(
      splitFields(create.fields, {}, "Base branch or commit is required").open,
    ).toBe(true);
  });

  it("marks the brief's advanced fields elsewhere", () => {
    const advanced = (id: string) =>
      command(id)
        .fields.filter((f) => f.advanced)
        .map((f) => f.key);
    expect(advanced("model")).toEqual(["account", "outside_pin"]);
    expect(advanced("orch")).toEqual(["wait_secs"]);
    expect(advanced("job")).toEqual(["slots", "timeout_secs"]);
    expect(advanced("park")).toEqual(["force"]);
    expect(advanced("events")).toEqual(["since_seq", "limit"]);
    expect(advanced("control")).toEqual(["since", "limit"]);
  });

  it("asks for no unguessable id in the palette", () => {
    const idOnly = commands.filter(
      (c) =>
        c.fields.length &&
        c.fields.every(
          (f) => !f.type || f.type === "text" || f.type === "number",
        ) &&
        c.fields.some((f) => /\bID\b/.test(f.label)),
    );
    expect(idOnly.map((c) => c.id)).toEqual([]);
  });
});

describe("the inspector's permission mode", () => {
  const picker = async (t: Thread, admin = true) => {
    const { PermissionModePicker } = await import("../shared/ThreadActions");
    return renderToStaticMarkup(
      createElement(PermissionModePicker, {
        id: "m",
        thread: t,
        admin,
        editable: true,
        rpc: async () => ({}),
        fail() {},
        onChanged() {},
      }),
    );
  };

  it("offers a Codex thread Codex's own modes, with admin", async () => {
    const html = await picker(thread({ runtime: "codex", mode: "on-request" }));
    expect(html).toContain("<select");
    expect(options(html).map(([value]) => value)).toContain(
      "danger-full-access",
    );
    expect(html).toMatch(
      /<option value="on-request" selected="">Codex: on-request</,
    );
    expect(await picker(thread({ runtime: "codex" }), false)).not.toContain(
      "on-request",
    );
  });

  it("shows the orchestrator's mode as text: the hub fixes it", async () => {
    const html = await picker(
      thread({ id: "o", is_orchestrator: true, mode: "dontAsk" }),
    );
    expect(html).not.toContain("<select");
    expect(html).toMatch(/<b id="m"[^>]*>Read only, never ask<\/b>/);
  });
});

describe("Other… in a picker, mounted", () => {
  it("swaps the list for a text box and back, keeping the focus on the field", async () => {
    vi.resetModules();
    const document = installFakeDOM();
    (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
    const { act, createElement: h, useState } = await import("react");
    const { createRoot } = await import("react-dom/client");
    const { FieldInput: Input } = await import("../shared/ui");
    const set = command("model");
    const changes: string[] = [];
    function Form() {
      const [value, setValue] = useState("");
      const resolved = resolveField(
        field(set, "runtime"),
        context(set, { values: { runtime: value } }),
      );
      return h(Input, {
        field: resolved,
        value,
        onChange: (next: string) => {
          changes.push(next);
          setValue(next);
        },
      });
    }
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container as unknown as Element);
    await act(async () => root.render(h(Form)));
    const control = () => document.getElementById("field-runtime") as any;
    expect(control().tagName).toBe("SELECT");
    const other = control()
      .querySelectorAll("option")
      .find((o: FakeElement) => o.textContent === "Other…");
    expect(other).toBeDefined();
    await act(async () => {
      control().value = other.value;
      control().dispatchEvent(new FakeEvent("change", { bubbles: true }));
    });
    expect(control().tagName).toBe("INPUT");
    expect(control().getAttribute("aria-label")).toBe("Runtime");
    expect(document.activeElement).toBe(control());
    await act(async () => {
      control().value = "acp:my-agent";
      control().dispatchEvent(new FakeEvent("input", { bubbles: true }));
    });
    expect(changes.at(-1)).toBe("acp:my-agent");
    const back = document.querySelector(
      "button[aria-label='Choose runtime from the list']",
    ) as FakeElement;
    await act(async () => {
      back.dispatchEvent(
        new FakeEvent("click", { bubbles: true, cancelable: true, button: 0 }),
      );
    });
    expect(control().tagName).toBe("SELECT");
    expect(changes.at(-1)).toBe("");
    expect(document.activeElement).toBe(control());
    await act(async () => root.unmount());
  });
});
