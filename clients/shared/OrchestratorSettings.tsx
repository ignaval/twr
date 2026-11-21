import { useEffect, useId, useState } from "react";
import { announce } from "./a11y";
import { modelChoices } from "./models";
import { OrchestratorAccessOptions } from "./OrchestratorAccessOptions";
import {
  SettingSwitch,
  settingValue,
  type SaveContext,
} from "./ThreadDefaults";
import type { Json } from "./types";
import { ModelSelect, Spinner } from "./ui";

/** What Apply sends to `orchestrator.use`: the runtime (with the current account while
 * the runtime stays the same), the model (none: the runtime's default), the effort while
 * the runtime stays the same, and a handoff summary. */
export function orchestratorSwitch(
  show: Json,
  runtime: string,
  model: string,
): Json {
  const same = runtime === show?.runtime;
  const spec =
    same && show?.account_shown && show?.account
      ? `${runtime}:${show.account}`
      : runtime;
  return {
    runtime: spec,
    ...(model ? { model } : {}),
    ...(same && show?.effort ? { effort: show.effort } : {}),
    summary: true,
  };
}

/** Settings › Agents › Orchestrator: its runtime and model, switched in place (with a
 * handoff), a fresh conversation, its access, and whether it starts with the hub. */
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
  };
  const [runtime, setRuntime] = useState(current.runtime);
  const [model, setModel] = useState(current.model);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  useEffect(() => {
    setRuntime(current.runtime);
    setModel(current.model);
  }, [current.runtime, current.model]);
  const runtimes: string[] = Array.isArray(show?.runtimes)
    ? show.runtimes.filter((r: unknown) => typeof r === "string")
    : [];
  if (runtime && !runtimes.includes(runtime)) runtimes.push(runtime);
  const changed = runtime !== current.runtime || model !== current.model;
  const apply = () => {
    setBusy(true);
    setStatus("");
    void rpc("orchestrator.use", orchestratorSwitch(show, runtime, model))
      .then((result) => {
        const text = `The orchestrator now runs on ${result?.runtime || runtime}${
          result?.model || model ? ` with ${result?.model || model}` : ""
        }.`;
        setStatus(text);
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
        The agent you talk to. Switching hands the conversation over with a
        summary.
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
                // Another runtime's models: start from its default.
                setModel(
                  e.target.value === current.runtime ? current.model : "",
                );
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
          <button
            type="button"
            className="primary"
            disabled={!changed || busy || !runtime}
            onClick={apply}
          >
            {busy && <Spinner />}
            Apply
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
