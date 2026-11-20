// The model and effort picker in the composer's footer row: a compact button naming what
// the composer's destination runs (the orchestrator, or the thread you dived into), and a
// popup above the composer (a bottom sheet on a phone) with the runtimes, their models and
// their efforts. The logic (rows, keys, which call) is in `modelPick.ts`.
import {
  useEffect,
  useId,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type RefObject,
} from "react";
import { Check } from "lucide-react";
import {
  currentPick,
  effortName,
  effortRows,
  modelRows,
  navigate,
  opensPicker,
  pickerRuntimes,
  planChange,
  runsCommandLine,
  runtimeMark,
  switchDetail,
  triggerText,
  type ModelRow,
  type Nav,
  type Pick,
} from "./modelPick";
import { Spinner } from "./ui";
import { announce } from "./a11y";
import type { Json, Thread } from "./types";
import "./model-picker.css";

/** A confirmation the app's dialog asks (`setConfirmation`): `alt` is a second way to go
 * on (the switch without its summary). */
export interface PickerConfirmation {
  title: string;
  detail: string;
  action: string;
  run: () => void;
  alt?: { action: string; run: () => void };
}

const mobile = () => document.documentElement.dataset.mobile === "true";

export function ModelPicker({
  target,
  kind,
  catalog,
  rpc,
  connected,
  operate,
  admin,
  confirm,
  withdraw,
  fail,
  input,
  queued = 0,
  switched,
}: {
  /** Where the composer sends: the orchestrator, or the dived thread. None: hidden. */
  target?: Thread;
  kind: "orchestrator" | "thread";
  /** `orchestrator.show` `models`. */
  catalog?: Json;
  rpc: (method: string, params?: Json) => Promise<Json>;
  connected: boolean;
  /** This device may change threads (its `operate` scope). */
  operate: boolean;
  /** This device may switch the orchestrator and move a thread to another runtime (its
   * `admin` scope; the local app always may). Without it a worker's model and effort on its
   * own runtime still change. */
  admin: boolean;
  confirm: (c: PickerConfirmation) => void;
  /** Takes back a confirmation `confirm` asked: false when it is no longer asked (it was
   * answered or cancelled). */
  withdraw: (c: PickerConfirmation) => boolean;
  fail: (error: unknown) => void;
  /** The message field: Esc gives it the focus back. */
  input: RefObject<HTMLTextAreaElement | null>;
  /** The orchestrator's queued messages, as the conversation shows them: a switch drops
   * them, and its confirmation says so. */
  queued?: number;
  /** Given `orchestrator.use`'s result once a switch went through (what it dropped). */
  switched?: (result: Json) => void;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [query, setQuery] = useState("");
  const [runtime, setRuntime] = useState("");
  const [pending, setPending] = useState<Pick>({ runtime: "" });
  const [custom, setCustom] = useState<string | null>(null);
  // Why the last choice was refused (a runtime this device may not move to, an empty custom
  // id), shown in the models column until the next choice.
  const [notice, setNotice] = useState("");
  const [nav, setNav] = useState<Nav>({
    column: "models",
    rail: 0,
    model: 0,
    effort: 0,
  });
  const root = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const customField = useRef<HTMLInputElement>(null);
  const id = useId();
  // What the picker is for: the thread, and what it runs. Its choices and the confirmation
  // it asks are planned against it, so when it changes underneath (another client switches
  // the orchestrator, sets or moves the thread, or deletes it) the popup closes and the
  // confirmation is taken back: the next choice starts from what runs now.
  const targetKey = target
    ? JSON.stringify([
        target.id,
        target.runtime,
        target.model || "",
        target.effort || "",
        target.account || "",
      ])
    : "";
  const latestKey = useRef(targetKey);
  latestKey.current = targetKey;
  const openedOn = useRef(targetKey);
  const asked = useRef<{ key: string; confirmation: PickerConfirmation }>(
    undefined,
  );
  const targetName = useRef("");
  if (target)
    targetName.current =
      kind === "orchestrator" ? "The orchestrator" : target.name;
  // Every orchestrator change is a switch (`orchestrator.use`), which is admin's.
  const switchBlocked = kind === "orchestrator" && !admin;
  const disabled = !target || !connected || !operate || switchBlocked || busy;
  const reason = !connected
    ? "Disconnected"
    : !operate
      ? "This device may not change models"
      : switchBlocked
        ? "Switching the orchestrator needs a device with admin access"
        : busy
          ? "Changing the model…"
          : undefined;

  const now = target ? currentPick(target) : { runtime: "" };
  const runtimes = pickerRuntimes(catalog, target?.runtime);
  const models = modelRows(
    catalog,
    runtime,
    query,
    runtime === now.runtime ? target?.model : undefined,
  );
  const efforts = effortRows(catalog, runtime);
  const locked = runsCommandLine(runtime);
  /** Another runtime than the thread's: a move, which this device may not make. */
  const blocked = (r: string) => !admin && r !== now.runtime;
  // A picked model the list doesn't have (a custom id): the custom row shows it, chosen.
  const customPicked =
    !!pending.model &&
    !modelRows(
      catalog,
      runtime,
      "",
      runtime === now.runtime ? target?.model : undefined,
    ).some((r) => r.kind === "model" && r.value === pending.model);

  const optionId = (column: string, i: number) => `${id}-${column}-${i}`;
  const focusOption = (option?: string) => {
    if (option) document.getElementById(option)?.focus();
  };

  const rowOf = (rows: ModelRow[], model?: string) =>
    Math.max(
      0,
      rows.findIndex((r) =>
        model ? r.kind === "model" && r.value === model : r.kind === "default",
      ),
    );

  const show = () => {
    if (!target || disabled) return;
    const pick = currentPick(target);
    const rows = modelRows(catalog, pick.runtime, "", target.model);
    setPending(pick);
    setRuntime(pick.runtime);
    setQuery("");
    setCustom(null);
    setNotice("");
    setNav({
      column: runsCommandLine(pick.runtime) ? "rail" : "models",
      rail: Math.max(0, runtimes.indexOf(pick.runtime)),
      model: rowOf(rows, pick.model),
      effort: Math.max(
        0,
        effortRows(catalog, pick.runtime).indexOf(pick.effort ?? null),
      ),
    });
    openedOn.current = targetKey;
    setOpen(true);
  };
  const close = (refocus = true) => {
    setOpen(false);
    setCustom(null);
    if (refocus) input.current?.focus();
  };

  // Ctrl+Shift+M (⌘+Shift+M on a Mac) opens it, unless a dialog is open.
  const showRef = useRef(show);
  showRef.current = show;
  useEffect(() => {
    const mac = ["darwin", "ios"].includes(
      document.documentElement.dataset.platform || "",
    );
    const keyboard = (event: KeyboardEvent) => {
      if (!opensPicker(event, mac)) return;
      const dialogs = [...document.querySelectorAll("[role=dialog]")];
      if (dialogs.some((d) => !d.classList.contains("model-picker-popup")))
        return;
      event.preventDefault();
      showRef.current();
    };
    window.addEventListener("keydown", keyboard);
    return () => window.removeEventListener("keydown", keyboard);
  }, []);
  // Focus goes to the search as it opens (on a phone, to the sheet: no keyboard pops up).
  // A runtime that runs its own command line has no search: the focus goes to its runtime
  // in the rail, and follows the highlighted row there (below).
  useEffect(() => {
    if (!open) return;
    if (mobile())
      root.current?.querySelector<HTMLElement>(".model-picker-popup")?.focus();
    else if (locked) focusOption(optionId("rail", nav.rail));
    else search.current?.focus();
  }, [open]);
  // From a runtime without a search to one with it: the search takes the focus once it is
  // enabled.
  useEffect(() => {
    if (open && !locked && !mobile()) search.current?.focus();
  }, [locked]);
  useEffect(() => {
    if (custom !== null) customField.current?.focus();
  }, [custom !== null]);
  // A click outside closes it.
  useEffect(() => {
    if (!open) return;
    const outside = (event: Event) => {
      if (
        event.target instanceof Node &&
        root.current &&
        !root.current.contains(event.target)
      )
        close(false);
    };
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [open]);
  // Gone (the destination changed, the hub went away): closed. Changed underneath (above):
  // closed, saying so, and a confirmation it asked taken back.
  useEffect(() => {
    const was = targetName.current;
    const changed = target ? `${was} changed` : `${was} is gone`;
    const stale = asked.current;
    if (stale && stale.key !== targetKey) {
      asked.current = undefined;
      if (withdraw(stale.confirmation))
        announce(`${changed}: the change you were confirming was cancelled.`);
    }
    if (!open) return;
    if (openedOn.current !== targetKey) {
      close(false);
      announce(`${changed}: the model picker closed. Open it again to pick.`);
    } else if (disabled) close(false);
  }, [disabled, targetKey]);

  const active =
    nav.column === "rail"
      ? optionId("rail", nav.rail)
      : nav.column === "models"
        ? locked
          ? undefined
          : optionId("models", nav.model)
        : optionId("efforts", nav.effort);
  // The highlighted row stays in view as the keys move it (the search keeps the focus, so
  // the list would not scroll by itself), in the rail and the efforts too, and as the
  // popup opens on the row in effect.
  useEffect(() => {
    if (!open || !active) return;
    document.getElementById(active)?.scrollIntoView?.({ block: "nearest" });
  }, [open, active]);
  // The focus on a row (no search to hold it: a runtime that runs its own command line)
  // follows the highlighted one.
  useEffect(() => {
    if (!open || !active) return;
    const focused = document.activeElement;
    if (
      focused instanceof HTMLElement &&
      focused.id !== active &&
      focused.matches(
        ".model-picker-popup [role=option], .model-picker-popup [role=radio]",
      )
    )
      focusOption(active);
  }, [open, active]);
  if (!target) return null;
  const shown = triggerText(target);
  const name = kind === "orchestrator" ? "the orchestrator" : target.name;

  const call = async (
    method: string,
    params: Json,
    then?: (result: Json) => void,
  ) => {
    setBusy(true);
    try {
      const result = await rpc(method, params);
      then?.(result);
    } catch (error) {
      fail(error);
    } finally {
      setBusy(false);
    }
  };
  const apply = (pick: Pick) => {
    close();
    const plan = planChange(kind, target, pick);
    if (plan.kind === "none") return;
    if (!admin && (plan.kind === "orchestrator" || plan.move)) {
      fail(
        new Error(`${needsAdmin(plan.kind)} needs a device with admin access`),
      );
      return;
    }
    // Asked against what runs now: it goes ahead only if that is still so.
    const key = targetKey;
    const ask = (c: PickerConfirmation) => {
      const current = (run: () => void) => () => {
        if (asked.current?.confirmation === confirmation)
          asked.current = undefined;
        if (latestKey.current !== key) {
          const whose =
            kind === "orchestrator" ? "The orchestrator" : target.name;
          fail(new Error(`${whose} changed before you confirmed: pick again`));
          return;
        }
        run();
      };
      const confirmation: PickerConfirmation = {
        ...c,
        run: current(c.run),
        ...(c.alt ? { alt: { ...c.alt, run: current(c.alt.run) } } : {}),
      };
      asked.current = { key, confirmation };
      confirm(confirmation);
    };
    const what = [
      pick.runtime,
      pick.model ? `model ${pick.model}` : "",
      pick.effort ? `effort ${effortName(pick.effort)}` : "",
    ]
      .filter(Boolean)
      .join(", ");
    if (plan.kind === "orchestrator") {
      ask({
        title: `Switch the orchestrator to ${what}?`,
        detail: switchDetail(queued),
        action: "Switch with summary",
        run: () => void call("orchestrator.use", plan.params, switched),
        alt: {
          action: "Switch without summary",
          run: () =>
            void call(
              "orchestrator.use",
              { ...plan.params, summary: false },
              switched,
            ),
        },
      });
      return;
    }
    if (plan.move) {
      ask({
        title: `Move ${target.name} to ${what}?`,
        detail:
          "A conversation cannot move between runtimes: the next message starts a new " +
          `${pick.runtime} session, with a handoff of what ${target.name} did so far.`,
        action: "Move thread",
        run: () => void call("thread.set_model", plan.params),
      });
      return;
    }
    void call("thread.set_model", plan.params);
  };
  const needsAdmin = (what: "orchestrator" | "thread") =>
    what === "orchestrator"
      ? "Switching the orchestrator"
      : `Moving ${target.name} to another runtime`;
  const chooseRuntime = (r: string) => {
    if (blocked(r)) {
      setNotice(
        `Moving ${target.name} to ${r} needs a device with admin access.`,
      );
      return;
    }
    setRuntime(r);
    setQuery("");
    setCustom(null);
    setNotice("");
    // Another runtime starts from its own defaults; the current one from what runs now.
    const pick: Pick = r === now.runtime ? { ...now } : { runtime: r };
    setPending(pick);
    const rows = modelRows(
      catalog,
      r,
      "",
      r === now.runtime ? target.model : undefined,
    );
    setNav({
      column: runsCommandLine(r) ? "rail" : "models",
      rail: Math.max(0, runtimes.indexOf(r)),
      model: rowOf(rows, pick.model),
      effort: Math.max(0, effortRows(catalog, r).indexOf(pick.effort ?? null)),
    });
    // No search to type in: the keys go on from the runtime picked.
    if (runsCommandLine(r)) {
      if (!mobile()) focusOption(optionId("rail", runtimes.indexOf(r)));
      return;
    }
    if (!mobile()) search.current?.focus();
  };
  const chooseModel = (model: string | undefined) => {
    const pick = { ...pending, runtime, model };
    setPending(pick);
    setCustom(null);
    setNotice("");
    // No efforts to pick (a runtime the catalog doesn't list, an older hub): chosen, the
    // effort as it is.
    if (efforts.length <= 1) {
      apply(pick);
      return;
    }
    setNav((n) => ({
      ...n,
      column: "efforts",
      effort: Math.max(0, efforts.indexOf(pick.effort ?? null)),
    }));
    if (!mobile()) search.current?.focus();
  };
  const chooseRow = (row: ModelRow) => {
    if (row.kind === "custom") {
      setNotice("");
      setCustom(query.trim() || (customPicked ? pending.model! : ""));
    } else chooseModel(row.kind === "model" ? row.value : undefined);
  };
  /** The custom id typed, or `undefined` (with the reason shown) while it is empty. */
  const typedId = () => {
    const model = (custom ?? "").trim();
    if (model) return model;
    setNotice("Type a model id first, or pick a model from the list.");
    customField.current?.focus();
    return undefined;
  };
  const commitCustom = () => {
    const model = typedId();
    if (model) chooseModel(model);
  };
  /** What is picked so far, the custom id typed included (it is chosen with an effort or
   * Done, as with Enter), or `undefined` while that id is empty. */
  const picked = (): Pick | undefined => {
    if (custom === null) return { ...pending, runtime };
    const model = typedId();
    return model ? { ...pending, runtime, model } : undefined;
  };
  const chooseEffort = (effort: string | null) => {
    const pick = picked();
    if (pick) apply({ ...pick, effort: effort ?? undefined });
  };
  const done = () => {
    if (locked) return apply({ runtime });
    const pick = picked();
    if (pick) apply(pick);
  };

  const keyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    const field = event.currentTarget as HTMLInputElement;
    const caretAtEnd =
      !(field instanceof HTMLInputElement) ||
      (field.selectionStart ?? 0) >= field.value.length;
    const moved = navigate(
      nav,
      event.key,
      {
        rail: runtimes.length,
        models: locked ? 0 : models.length,
        efforts: locked ? 0 : efforts.length,
      },
      !query,
      caretAtEnd,
    );
    if (!moved) return;
    event.preventDefault();
    event.stopPropagation();
    setNav(moved.nav);
    if (moved.effect === "close") close();
    if (moved.effect === "pick-runtime" && runtimes[moved.nav.rail])
      chooseRuntime(runtimes[moved.nav.rail]);
    if (moved.effect === "pick-model" && !locked && models[moved.nav.model])
      chooseRow(models[moved.nav.model]);
    if (moved.effect === "pick-effort" && !locked)
      chooseEffort(efforts[moved.nav.effort] ?? null);
  };
  // The model the efforts are for: the one picked, else the default (as the runtime reported
  // it, on the current runtime).
  const effortFor =
    pending.model ||
    (runtime === now.runtime && !now.model ? target.model_actual : undefined) ||
    "the default model";

  return (
    <div className="model-picker" ref={root}>
      <button
        type="button"
        className="model-picker-trigger"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={shown.label}
        title={reason ? `${shown.label} (${reason})` : shown.label}
        disabled={disabled}
        onClick={() => (open ? close() : show())}
      >
        <span className="model-picker-mark" aria-hidden="true">
          {runtimeMark(target.runtime)}
        </span>
        <span className="model-picker-model">{shown.model}</span>
        <span className="model-picker-effort">{shown.effort}</span>
        {busy && <Spinner />}
      </button>
      {open && (
        <div
          className="model-picker-popup"
          role="dialog"
          aria-label={`Model and effort for ${name}`}
          tabIndex={-1}
          onKeyDown={(event) => {
            // The popup and its rows (the search and the custom id field handle their own
            // keys); elsewhere (Cancel, Done) Esc still closes it.
            const from = event.target as HTMLElement;
            if (
              from === event.currentTarget ||
              from.matches("[role=option], [role=radio]")
            )
              keyDown(event);
            else if (event.key === "Escape") {
              event.preventDefault();
              event.stopPropagation();
              close();
            }
          }}
        >
          <div
            className="model-picker-rail"
            role="listbox"
            aria-label="Runtime"
          >
            {runtimes.map((r, i) => (
              <button
                type="button"
                role="option"
                key={r}
                id={optionId("rail", i)}
                tabIndex={-1}
                aria-selected={r === runtime}
                aria-disabled={blocked(r) || undefined}
                className={
                  nav.column === "rail" && nav.rail === i ? "active" : ""
                }
                title={
                  r === now.runtime
                    ? `${r} (current)`
                    : blocked(r)
                      ? `${r}: moving to it needs a device with admin access`
                      : r
                }
                onClick={() => chooseRuntime(r)}
              >
                <span className="model-picker-mark" aria-hidden="true">
                  {runtimeMark(r)}
                </span>
                <span className="model-picker-rail-name">{r}</span>
                {r === now.runtime && (
                  <Check size={12} aria-hidden="true" className="current" />
                )}
              </button>
            ))}
          </div>
          <div className="model-picker-models">
            <input
              ref={search}
              className="model-picker-search"
              type="search"
              aria-label="Search models"
              placeholder="Search models…"
              role="combobox"
              aria-expanded="true"
              aria-controls={`${id}-models`}
              aria-activedescendant={active}
              autoComplete="off"
              spellCheck={false}
              value={query}
              disabled={locked}
              onChange={(event) => {
                setQuery(event.target.value);
                setNav((n) => ({ ...n, column: "models", model: 0 }));
              }}
              onKeyDown={keyDown}
            />
            <p className="model-picker-notice" role="status">
              {notice}
            </p>
            {locked ? (
              <p className="model-picker-note" id={`${id}-models`}>
                {target.name} runs its own command line: it has no model or
                effort.{" "}
                {admin
                  ? "Pick another runtime to move it to an agent."
                  : "Moving it to another runtime needs a device with admin access."}
              </p>
            ) : (
              <div
                role="listbox"
                id={`${id}-models`}
                aria-label={`Models for ${runtime}`}
              >
                {models.map((row, i) =>
                  row.kind === "custom" && custom !== null ? (
                    <div className="model-picker-custom" key="custom">
                      <input
                        ref={customField}
                        type="text"
                        aria-label="Custom model id"
                        placeholder="Model id"
                        autoComplete="off"
                        spellCheck={false}
                        value={custom}
                        onChange={(event) => setCustom(event.target.value)}
                        enterKeyHint="done"
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault();
                            event.stopPropagation();
                            commitCustom();
                          }
                          if (event.key === "Escape") {
                            event.preventDefault();
                            event.stopPropagation();
                            setCustom(null);
                            setNotice("");
                            search.current?.focus();
                          }
                        }}
                      />
                      <button
                        type="button"
                        aria-label="Use this model id"
                        onClick={commitCustom}
                      >
                        Use
                      </button>
                    </div>
                  ) : (
                    <button
                      type="button"
                      role="option"
                      key={row.kind === "model" ? row.value : row.kind}
                      id={optionId("models", i)}
                      tabIndex={-1}
                      aria-selected={
                        row.kind === "model"
                          ? pending.model === row.value
                          : row.kind === "default"
                            ? !pending.model
                            : customPicked
                      }
                      className={
                        nav.column === "models" && nav.model === i
                          ? "active"
                          : ""
                      }
                      title={row.kind === "model" ? row.description : undefined}
                      onClick={() => {
                        setNav((n) => ({ ...n, column: "models", model: i }));
                        chooseRow(row);
                      }}
                    >
                      <span className="model-picker-name">
                        {row.kind === "default"
                          ? "Default"
                          : row.kind === "custom"
                            ? "Custom model id…"
                            : row.value}
                      </span>
                      <span className="model-picker-about">
                        {row.kind === "default"
                          ? "the runtime's own"
                          : row.kind === "model"
                            ? row.description
                            : customPicked
                              ? pending.model
                              : ""}
                      </span>
                      {runtime === now.runtime &&
                        (row.kind === "model"
                          ? now.model === row.value
                          : row.kind === "default" && !now.model) && (
                          <Check
                            size={14}
                            aria-label="current"
                            role="img"
                            className="current"
                          />
                        )}
                    </button>
                  ),
                )}
              </div>
            )}
          </div>
          <div className="model-picker-efforts">
            <header>
              <b>Effort</b>
              <span>{locked ? "" : effortFor}</span>
            </header>
            <div
              role="radiogroup"
              aria-label={`Effort for ${effortFor}`}
              aria-disabled={locked || undefined}
            >
              {(locked ? [] : efforts).map((e, i) => (
                <button
                  type="button"
                  role="radio"
                  key={e ?? "default"}
                  id={optionId("efforts", i)}
                  tabIndex={-1}
                  aria-checked={(pending.effort ?? null) === e}
                  className={
                    nav.column === "efforts" && nav.effort === i ? "active" : ""
                  }
                  onClick={() => chooseEffort(e)}
                >
                  <span className="model-picker-radio" aria-hidden="true" />
                  <span className="model-picker-name">
                    {e ? effortName(e) : "Default"}
                  </span>
                  {runtime === now.runtime && (now.effort ?? null) === e && (
                    <Check
                      size={14}
                      aria-label="current"
                      role="img"
                      className="current"
                    />
                  )}
                </button>
              ))}
              {!locked && efforts.length <= 1 && (
                <p className="model-picker-note">
                  {runtime} takes no effort setting here.
                </p>
              )}
            </div>
            <footer>
              <button type="button" onClick={() => close()}>
                Cancel
              </button>
              <button type="button" className="primary" onClick={done}>
                Done
              </button>
            </footer>
          </div>
        </div>
      )}
    </div>
  );
}
