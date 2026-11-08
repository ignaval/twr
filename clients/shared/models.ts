/** Model and reasoning-effort choices for the pickers, from the hub's catalog
 * (`orchestrator.show` `models`: `{<runtime>: {models: [{id, description}], efforts}}`).
 * The list is a choice, not a check: a runtime takes other ids, so every picker keeps a
 * custom id, and leaving the model unset runs the runtime's own default. */
import type { Json, Thread } from "./types";

export interface Choice {
  value: string;
  description?: string;
}

/** The runtime part of a runtime spec (`claude-code:work` → `claude-code`). */
export function baseRuntime(spec?: string): string {
  if (!spec) return "";
  const trimmed = spec.trim();
  // `acp:<command>` and `pty:<command>` name a command, not an account.
  if (trimmed.startsWith("acp:") || trimmed.startsWith("pty:")) return trimmed;
  return trimmed.split(":")[0];
}

/** The models the catalog lists for `runtime` (a spec), in its order. Empty for a runtime
 * it doesn't list (a generic ACP agent, an older hub). */
export function modelChoices(catalog: Json, runtime?: string): Choice[] {
  const models = catalog?.[baseRuntime(runtime)]?.models;
  if (!Array.isArray(models)) return [];
  return models
    .filter((m: Json) => typeof m?.id === "string")
    .map((m: Json) => ({
      value: m.id,
      description:
        typeof m.description === "string" ? m.description : undefined,
    }));
}

/** The reasoning efforts the catalog lists for `runtime` (a spec). */
export function effortChoices(catalog: Json, runtime?: string): Choice[] {
  const efforts = catalog?.[baseRuntime(runtime)]?.efforts;
  if (!Array.isArray(efforts)) return [];
  return efforts
    .filter((e: Json) => typeof e === "string")
    .map((value: string) => ({ value }));
}

/** The runtime a form's model and effort are for: the runtime it names, else the thread's
 * (`thread.set_model`, by id or name), else the orchestrator's (`orchestrator.use`), else the
 * hub's default runtime (`fallback`). */
export function formRuntime(
  command: { id: string },
  values: Json,
  threads: Thread[],
  fallback?: string,
): string | undefined {
  if (typeof values?.runtime === "string" && values.runtime.trim())
    return values.runtime;
  if (command.id === "model" && values?.thread) {
    const t = threads.find(
      (t) => t.id === values.thread || t.name === values.thread,
    );
    if (t) return t.runtime;
  }
  if (command.id === "orch") {
    const o = threads.find((t) => t.is_orchestrator);
    if (o) return o.runtime;
  }
  return fallback;
}
