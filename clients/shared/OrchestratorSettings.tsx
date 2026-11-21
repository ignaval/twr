import { useEffect, useId, useState } from "react";
import { announce } from "./a11y";
import { effortChoices, modelChoices } from "./models";
import { changeProblem } from "./modelPick";
import { OrchestratorAccessOptions } from "./OrchestratorAccessOptions";
import {
  SettingSwitch,
  settingValue,
  type SaveContext,
} from "./ThreadDefaults";
import type { Json } from "./types";
import { ModelSelect, Spinner } from "./ui";

/** What Apply does (as the composer's model picker does, `modelPick.ts`): on the runtime it
 * runs, the orchestrator's model and effort change in place, `thread.set_model` on its
 * thread with only what changed (`default` for a setting cleared), on the account and agent
 * it runs; another runtime (or no orchestrator yet) is a switch, `orchestrator.use`, which
 * opens the Switch orchestrator form with the choice filled in. */
export function orchestratorChange(
  show: Json,
  runtime: string,
  model: string,
  effort: string,
):
  | { kind: "none" }
  | { kind: "in-place"; params: Json }
  | { kind: "switch"; initial: Json } {
  const same = runtime === show?.runtime;
  if (!same || !show?.thread)
    return {
      kind: "switch",
      initial: {
        runtime,
        ...(model ? { model } : {}),
        ...(effort ? { effort } : {}),
      },
    };
  const params: Json = { thread: show.thread };
  if (model !== (show.model || "")) params.model = model || "default";
  if (effort !== (show.effort || "")) params.effort = effort || "default";
  if (!("model" in params) && !("effort" in params)) return { kind: "none" };
  return { kind: "in-place", params };
}

/** Settings › Agents › Orchestrator: its runtime, model and effort (changed in place on its
 * runtime, switched with a handoff to another), a fresh conversation, its access, and
 * whether it starts with the hub. */
export function OrchestratorSettings({
  rpc,
  action,
  show,
  reload,
  loaded,
  context,
  orchestratorAccess,
  refreshKey,
  infoChanged,
}: {
  rpc: (method: string, params?: Json) => Promise<Json>;
  action: (id: string, initial?: Json) => void;
  /** `orchestrator.show`, if read. */
  show?: Json;
  /** Read `orchestrator.show` again. */
  reload: () => void;
  /** The config file's settings (autostart), if read. */
  loaded?: Json;
  context: SaveContext;
  orchestratorAccess?: string;
  refreshKey?: number;
  infoChanged?: () => void;
}) {
  const ids = useId();
  const current = {
    runtime: show?.runtime || "",
    model: show?.model || "",
    effort: show?.effort || "",
  };
  const [runtime, setRuntime] = useState(current.runtime);
  const [model, setModel] = useState(current.model);
  const [effort, setEffort] = useState(current.effort);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  useEffect(() => {
    setRuntime(current.runtime);
    setModel(current.model);
    setEffort(current.effort);
  }, [current.runtime, current.model, current.effort]);
  const runtimes: string[] = Array.isArray(show?.runtimes)
    ? show.runtimes.filter((r: unknown) => typeof r === "string")
    : [];
  if (runtime && !runtimes.includes(runtime)) runtimes.push(runtime);
  const efforts = effortChoices(show?.models, runtime);
  const plan = orchestratorChange(show, runtime, model, effort);
  const apply = () => {
    if (plan.kind === "none") return;
    // Another runtime is a new orchestrator session: the Switch orchestrator form says so,
    // and what the switch drops is offered back from there.
    if (plan.kind === "switch") {
      action("orch", plan.initial);
      return;
    }
    setBusy(true);
    setStatus("");
    void rpc("thread.set_model", plan.params)
      .then((result) => {
        const said = (key: "model" | "effort", picked: string) =>
          (result && key in result ? result[key] : picked) || "";
        const now = said("model", model) || "its runtime's default model";
        const at = said("effort", effort);
        const text =
          `The orchestrator now runs ${now}` +
          (at ? ` at ${at} effort` : "") +
          ", on the same session.";
        const problem = changeProblem(result);
        setStatus(problem ? `${text} ${problem}` : text);
        announce(text);
        infoChanged?.();
        reload();
      })
      .catch(context.fail)
      .finally(() => setBusy(false));
  };
  return (
    <section className="settings-section orchestrator-settings">
      <h2>Orchestrator</h2>
      <p>
        The agent you talk to. A model or effort changes in place; another
        runtime is a new orchestrator that gets a handoff summary.
      </p>
      {show && runtimes.length > 0 && (
        <div className="orchestrator-pick">
          <div className="field">
            <label htmlFor={`${ids}-runtime`}>Runtime</label>
            <select
              id={`${ids}-runtime`}
              value={runtime}
              disabled={busy}
              onChange={(e) => {
                setRuntime(e.target.value);
                // Another runtime's models and efforts: start from its defaults.
                const same = e.target.value === current.runtime;
                setModel(same ? current.model : "");
                setEffort(same ? current.effort : "");
              }}
            >
              {runtimes.map((r) => (
                <option
                  key={r}
                  value={r}
                  disabled={show?.installed?.[r] === false && r !== runtime}
                >
                  {r}
                  {show?.installed?.[r] === false ? " (not installed)" : ""}
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor={`${ids}-model`}>Model</label>
            <ModelSelect
              id={`${ids}-model`}
              value={model}
              onChange={setModel}
              choices={modelChoices(show?.models, runtime)}
            />
          </div>
          {efforts.length > 0 && (
            <div className="field">
              <label htmlFor={`${ids}-effort`}>Effort</label>
              <ModelSelect
                id={`${ids}-effort`}
                value={effort}
                onChange={setEffort}
                choices={efforts}
                custom={false}
              />
            </div>
          )}
          <button
            type="button"
            className="primary"
            disabled={plan.kind === "none" || busy || !runtime}
            aria-haspopup={plan.kind === "switch" ? "dialog" : undefined}
            onClick={apply}
          >
            {busy && <Spinner />}
            {plan.kind === "switch" ? "Switch…" : "Apply"}
          </button>
        </div>
      )}
      <p className="success-text" role="status">
        {status}
      </p>
      <div className="manager-actions">
        {!(show && runtimes.length > 0) && (
          // An older hub lists no runtimes here: the palette's form still switches.
          <button onClick={() => action("orch")}>Switch orchestrator…</button>
        )}
        <button onClick={() => action("clear")}>Fresh conversation…</button>
      </div>
      <OrchestratorAccessOptions
        rpc={rpc}
        live={orchestratorAccess}
        refreshKey={refreshKey}
        changed={infoChanged}
      />
      {settingValue(loaded, ["orchestrator"], "autostart") !== undefined && (
        <SettingSwitch
          table={["orchestrator"]}
          name="autostart"
          label="Start with the hub"
          detail="Otherwise it starts with your first message."
          initial={settingValue(loaded, ["orchestrator"], "autostart")}
          context={context}
        />
      )}
    </section>
  );
}
