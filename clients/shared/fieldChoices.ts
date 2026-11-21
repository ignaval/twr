/** The choices a command form's pickers offer (`Field.type` a `PickerType`), built from what
 * the app already has (threads, `hub.info`) or reads once the form opens (`pickerSources`).
 * A picker whose list could not be read stays a text box, as before. */
import type { Command, Field } from "./commands";
import { effortChoices, formRuntime, modelChoices } from "./models";
import { permissionChoices, type Choice } from "./labels";
import type { Json, Thread } from "./types";

export type { Choice };

/** The runtimes every hub has, before the ones it reports installed. */
export const BUILT_IN_RUNTIMES = ["claude-code", "codex", "grok-build"];

/** A device's access, cumulative: each level includes the ones above it. The same four
 * levels Connections.tsx's Device access offers. */
export const SCOPE_CHOICES: Choice[] = [
  { value: "read", label: "Read only" },
  { value: "read operate", label: "Read and operate" },
  { value: "read operate terminal", label: "Read, operate and terminals" },
  {
    value: "read operate terminal admin",
    label: "Read, operate, terminals and settings",
  },
];

/** Usage periods `usage.threads` `since` accepts; empty is all time. */
export const SINCE_CHOICES: Choice[] = [
  { value: "1d", label: "Last 24 hours" },
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
];

/** The list RPC each picker reads when the form opens (none: built from app state). */
const SOURCES: Partial<Record<NonNullable<Field["type"]>, string>> = {
  account: "account.list",
  speaker: "speech.status",
  voice: "speech.status",
  job: "job.list",
  "trash-item": "trash.list",
  watch: "orchestrator.watches",
  device: "device.list",
  "memory-note": "memory.list",
};

/** The list RPCs `command`'s pickers need, once each. */
export function pickerSources(command: Command): string[] {
  if (command.cli) return [];
  return [
    ...new Set(
      command.fields
        .map((field) => field.type && SOURCES[field.type])
        .filter((method): method is string => !!method),
    ),
  ];
}

/** Adds `current` to `choices` when it is set and not among them, so a value from a slash
 * command or another screen still shows (and is what the form sends). */
export function withCurrent(choices: Choice[], current?: unknown): Choice[] {
  if (typeof current !== "string" || !current) return choices;
  return choices.some((c) => c.value === current)
    ? choices
    : [...choices, { value: current, label: current }];
}

const words = (status?: string) => (status ? status.replaceAll("_", " ") : "");

/** The workspace's threads, by id (what every thread RPC takes, as it takes a name), the
 * orchestrator last. A name or alias the form was opened with picks its thread. */
export function threadChoices(threads: Thread[]): Choice[] {
  return [...threads]
    .sort((a, b) => Number(a.is_orchestrator) - Number(b.is_orchestrator))
    .map((t) => ({
      value: t.id,
      label: [
        t.is_orchestrator ? "Orchestrator" : t.name,
        words(t.status_label || t.status),
      ]
        .filter(Boolean)
        .join(" · "),
    }));
}

/** The thread `value` names: by id, name or alias. */
export function findThread(threads: Thread[], value: unknown) {
  if (typeof value !== "string" || !value) return undefined;
  return (
    threads.find((t) => t.id === value) ||
    threads.find((t) => t.name === value || t.aliases?.includes(value))
  );
}

/** Installed runtimes: the built-in three, then those `hub.info` reports and the ones
 * threads use (a configured preset). */
export function runtimeChoices(info: Json, threads: Thread[] = []): Choice[] {
  const names = [
    ...BUILT_IN_RUNTIMES,
    ...(Array.isArray(info?.agents) ? info.agents : []).map(
      (a: Json) => a?.runtime,
    ),
    ...threads.map((t) => t.runtime),
  ].filter((name): name is string => typeof name === "string" && !!name);
  return [...new Set(names)].map((value) => ({ value, label: value }));
}

/** The hub's boxes (`hub.info` `boxes`), "local" first. */
export function boxChoices(info: Json): Choice[] {
  const names = (Array.isArray(info?.boxes) ? info.boxes : [])
    .map((b: Json) => (typeof b === "string" ? b : b?.name))
    .filter((name: unknown): name is string => typeof name === "string");
  const states = new Map<string, string>(
    (Array.isArray(info?.boxes) ? info.boxes : [])
      .filter((b: Json) => b && typeof b === "object")
      .map((b: Json) => [b.name, b.online === false ? "offline" : ""]),
  );
  return [...new Set(["local", ...names])].map((value) => ({
    value,
    label: states.get(value) ? `${value} · ${states.get(value)}` : value,
  }));
}

/** `account.list`'s accounts for `runtime` (every runtime's, named with it, when none is
 * chosen). */
export function accountChoices(list: Json, runtime?: string): Choice[] {
  const groups = list?.runtimes;
  if (!groups || typeof groups !== "object") return [];
  const base = runtime?.split(":")[0];
  return Object.entries(groups).flatMap(([name, group]: [string, Json]) =>
    base && name !== base
      ? []
      : (Array.isArray(group?.accounts) ? group.accounts : [])
          .filter((a: Json) => a && typeof a.name === "string" && !a.removed)
          .map((a: Json) => ({
            value: a.name,
            label: base ? a.name : `${a.name} · ${name}`,
            description: a.login ? words(a.login) : undefined,
          })),
  );
}

/** `speech.status`'s speakers. */
export function speakerChoices(status: Json): Choice[] {
  return (Array.isArray(status?.speakers) ? status.speakers : [])
    .filter((s: Json) => typeof s?.name === "string")
    .map((s: Json) => ({
      value: s.name,
      label: s.engine ? `${s.name} · ${s.engine}` : s.name,
    }));
}

/** The voices the speakers use now (`speaker`'s only, when one is chosen): the hub lists
 * no others, so a voice field suggests these and takes any name. */
export function voiceSuggestions(status: Json, speaker?: string): string[] {
  const names: string[] = (
    Array.isArray(status?.speakers) ? status.speakers : []
  )
    .filter((s: Json) => !speaker || s?.name === speaker)
    .map((s: Json) => s?.voice)
    .filter((v: unknown): v is string => typeof v === "string" && !!v);
  return [...new Set(names)];
}

/** `job.list`'s jobs: name, thread and state. */
export function jobChoices(list: Json): Choice[] {
  return (Array.isArray(list) ? list : [])
    .filter((j: Json) => typeof j?.job === "string")
    .map((j: Json) => ({
      value: j.job,
      label: [j.name || String(j.job), j.thread_name, j.state]
        .filter(Boolean)
        .join(" · "),
    }));
}

/** `trash.list`'s items: the thread's name and when it was deleted. */
export function trashChoices(list: Json): Choice[] {
  return (Array.isArray(list?.items) ? list.items : [])
    .filter((t: Json) => typeof t?.item === "string" && !t.incomplete)
    .map((t: Json) => ({
      value: t.item,
      label: [
        t.thread_name || t.item,
        t.deleted_at && `deleted ${new Date(t.deleted_at).toLocaleString()}`,
      ]
        .filter(Boolean)
        .join(" · "),
    }));
}

/** `orchestrator.watches`: each watch by its thread and note, then every watch at once. */
export function watchChoices(list: Json): Choice[] {
  const rows = (Array.isArray(list?.watches) ? list.watches : [])
    .filter((w: Json) => typeof w?.watch === "string")
    .map((w: Json) => ({
      value: w.watch,
      label:
        [w.thread_name || w.thread, clip(w.note)].filter(Boolean).join(" · ") ||
        w.watch,
    }));
  return rows.length ? [...rows, { value: "all", label: "Every watch" }] : [];
}

/** The messages held until other threads finish (each thread's `queued_after`). */
export function heldChoices(threads: Thread[]): Choice[] {
  return threads.flatMap((t) =>
    (Array.isArray(t.queued_after) ? t.queued_after : [])
      .filter((h: Json) => typeof h?.id === "string")
      .map((h: Json) => ({
        value: h.id,
        label: [
          `To ${t.name}`,
          Array.isArray(h.after) && h.after.length
            ? `after ${h.after.join(", ")}`
            : "",
          clip(h.prompt),
        ]
          .filter(Boolean)
          .join(" · "),
      })),
  );
}

/** A long note or prompt cut to fit an option. */
function clip(text: unknown, max = 60) {
  if (typeof text !== "string") return "";
  const line = text.replace(/\s+/g, " ").trim();
  return line.length > max ? line.slice(0, max - 1) + "…" : line;
}

/** `device.list`'s paired devices, by name and access. */
export function deviceChoices(list: Json): Choice[] {
  return (Array.isArray(list) ? list : [])
    .filter((d: Json) => typeof d?.id === "string")
    .map((d: Json) => ({
      value: d.id,
      label: [
        d.name || d.id,
        Array.isArray(d.scopes) ? scopeLabel(d.scopes) : "",
      ]
        .filter(Boolean)
        .join(" · "),
    }));
}

/** A device's scopes in words (the widest level it has). */
export function scopeLabel(scopes: string[]): string {
  const level = [...SCOPE_CHOICES]
    .reverse()
    .find((c) => scopes.includes(c.value.split(" ").at(-1)!));
  return level?.label ?? scopes.join(", ");
}

/** `memory.list`'s notes, by name. */
export function memoryChoices(list: Json): Choice[] {
  return (Array.isArray(list?.notes) ? list.notes : [])
    .filter((n: Json) => typeof n?.name === "string")
    .map((n: Json) => ({
      value: n.name,
      label: n.description ? `${n.name} — ${n.description}` : n.name,
    }));
}

export interface PickerContext {
  command: Command;
  values: Record<string, Json>;
  threads: Thread[];
  /** `hub.info`. */
  info?: Json;
  /** The device may change settings (`admin`). */
  admin: boolean;
  /** The hub's model catalog (`orchestrator.show` `models`). */
  modelCatalog?: Json;
  /** What each of `pickerSources` read: undefined while loading, null when it failed. */
  lists: Record<string, Json>;
}

/** `field` as `FieldInput` draws it in `ctx`'s form: a picker with its choices (loading,
 * or none when its list could not be read, so it stays a text box). */
export function resolveField(field: Field, ctx: PickerContext): Field {
  const { command, values, threads, info, lists } = ctx;
  const listed = (method: string, build: (list: Json) => Choice[]): Field => {
    const list = lists[method];
    if (list === undefined) return { ...field, loading: true, choices: [] };
    if (list === null) return { ...field, type: "text", custom: false };
    // A custom picker shows a value its list lacks as typed text instead.
    const choices = build(list);
    return {
      ...field,
      choices: field.custom ? choices : withCurrent(choices, values[field.key]),
    };
  };
  // A CLI form's fields are the CLI's own: text, with the known runtimes suggested.
  if (command.cli)
    return field.key === "runtime" && (!field.type || field.type === "text")
      ? {
          ...field,
          options: runtimeChoices(info, threads).map((c) => c.value),
        }
      : field;
  switch (field.type) {
    case "model":
    case "effort": {
      const runtime = formRuntime(
        command,
        values,
        threads,
        info?.orchestrator?.configured?.explicit === false
          ? info.orchestrator.configured.runtime
          : undefined,
      );
      return {
        ...field,
        choices: (field.type === "model" ? modelChoices : effortChoices)(
          ctx.modelCatalog,
          runtime,
        ),
      };
    }
    case "thread": {
      const choices = threadChoices(threads);
      const known = findThread(threads, values[field.key]);
      return {
        ...field,
        // A name or alias picks its thread's entry; anything else still shows.
        choices: known ? choices : withCurrent(choices, values[field.key]),
      };
    }
    case "runtime": {
      const choices = runtimeChoices(info, threads);
      return {
        ...field,
        choices: field.custom
          ? choices
          : withCurrent(choices, values[field.key]),
      };
    }
    case "box":
      return {
        ...field,
        choices: withCurrent(boxChoices(info), values[field.key]),
      };
    case "permission-mode": {
      // With admin: twr's modes, the runtime's own and Other… for any id it takes. Without,
      // the narrow ones only (the hub refuses the rest), keeping the current one shown.
      const runtime =
        (typeof values.runtime === "string" && values.runtime.trim()) ||
        findThread(threads, values.thread)?.runtime;
      return {
        ...field,
        custom: ctx.admin,
        placeholder: "A mode id the runtime takes, such as on-request",
        choices: permissionChoices(
          ctx.admin ? undefined : values[field.key],
          ctx.admin,
          runtime,
        ).map((c) => ({
          value: c.value,
          label: c.label,
          description: c.description,
        })),
      };
    }
    case "scopes":
      return {
        ...field,
        choices: withCurrent(SCOPE_CHOICES, values[field.key]),
      };
    case "account": {
      const runtime =
        (typeof values.runtime === "string" && values.runtime) ||
        findThread(threads, values.thread)?.runtime;
      return listed("account.list", (list) => accountChoices(list, runtime));
    }
    case "speaker":
      return listed("speech.status", speakerChoices);
    case "voice": {
      // Typed, with the speakers' current voices suggested.
      const status = lists["speech.status"];
      return {
        ...field,
        type: "text",
        options: status
          ? voiceSuggestions(status, values.speaker || undefined)
          : undefined,
      };
    }
    case "job":
      return listed("job.list", jobChoices);
    case "trash-item":
      return listed("trash.list", trashChoices);
    case "watch":
      return listed("orchestrator.watches", watchChoices);
    case "held":
      return {
        ...field,
        choices: withCurrent(heldChoices(threads), values[field.key]),
      };
    case "device":
      return listed("device.list", deviceChoices);
    case "memory-note":
      return listed("memory.list", memoryChoices);
  }
  return field;
}

/** The value a field starts at: what it was opened with, else its initial value. */
function startValue(field: Field) {
  return field.initial ?? (field.type === "boolean" ? false : "");
}

/** A form's fields in two parts: the essential ones, shown, and the advanced ones, under
 * More options. It opens by itself when an advanced field holds something other than its
 * start value, or `error` names one. */
export function splitFields(
  fields: Field[],
  values: Record<string, Json>,
  error = "",
): { essential: Field[]; advanced: Field[]; open: boolean } {
  const essential = fields.filter((f) => !f.advanced || f.required);
  const advanced = fields.filter((f) => f.advanced && !f.required);
  const open = advanced.some(
    (f) =>
      (values[f.key] !== undefined && values[f.key] !== startValue(f)) ||
      (!!error && error.includes(f.label)),
  );
  return { essential, advanced, open };
}
