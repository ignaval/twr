import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
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

  it("offers installed runtimes, the built-ins first, and keeps a custom one", () => {
    expect(
      runtimeChoices({ agents: [{ runtime: "codex" }, { runtime: "pi" }] }, [
        thread({ runtime: "acp:gemini" }),
      ]).map((c) => c.value),
    ).toEqual(["claude-code", "codex", "grok-build", "pi", "acp:gemini"]);
    const set = command("model");
    const runtime = resolveField(
      field(set, "runtime"),
      context(set, { values: { runtime: "fake" } }),
    );
    expect(runtime.keep).toBe(true);
    expect(runtime.choices?.at(-1)?.value).toBe("fake");
    const html = renderToStaticMarkup(
      createElement(FieldInput, { field: runtime, value: "", onChange() {} }),
    );
    expect(options(html)[0]).toEqual(["", "Keep current"]);
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
    // New thread's mode is the same labelled picker.
    expect(field(command("new"), "mode").type).toBe("permission-mode");
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
