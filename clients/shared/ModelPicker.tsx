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
  triggerText,
  type ModelRow,
  type Nav,
  type Pick,
} from "./modelPick";
import { Spinner } from "./ui";
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
  confirm,
  fail,
  input,
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
  confirm: (c: PickerConfirmation) => void;
  fail: (error: unknown) => void;
  /** The message field: Esc gives it the focus back. */
  input: RefObject<HTMLTextAreaElement | null>;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [query, setQuery] = useState("");
  const [runtime, setRuntime] = useState("");
  const [pending, setPending] = useState<Pick>({ runtime: "" });
  const [custom, setCustom] = useState<string | null>(null);
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
  const disabled = !target || !connected || !operate || busy;
  const reason = !connected
    ? "Disconnected"
    : !operate
      ? "This device may not change models"
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
    setNav({
      column: runsCommandLine(pick.runtime) ? "rail" : "models",
      rail: Math.max(0, runtimes.indexOf(pick.runtime)),
      model: rowOf(rows, pick.model),
      effort: Math.max(
        0,
        effortRows(catalog, pick.runtime).indexOf(pick.effort ?? null),
      ),
    });
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
  useEffect(() => {
    if (!open) return;
    if (mobile())
      root.current?.querySelector<HTMLElement>(".model-picker-popup")?.focus();
    else search.current?.focus();
  }, [open]);
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
  // Gone (the destination changed, the hub went away): closed.
  useEffect(() => {
    if (disabled && open) close(false);
  }, [disabled, target?.id]);

  if (!target) return null;
  const shown = triggerText(target);
  const name = kind === "orchestrator" ? "the orchestrator" : target.name;

  const call = async (method: string, params: Json) => {
    setBusy(true);
    try {
      await rpc(method, params);
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
    const what = [
      pick.runtime,
      pick.model ? `model ${pick.model}` : "",
      pick.effort ? `effort ${effortName(pick.effort)}` : "",
    ]
      .filter(Boolean)
      .join(", ");
    if (plan.kind === "orchestrator") {
      confirm({
        title: `Switch the orchestrator to ${what}?`,
        detail:
          "This starts a new orchestrator session that begins with a handoff: what the log " +
          "shows, and with the summary, the current orchestrator's own (it is asked first, " +
          "and waited for up to 60 seconds while it is busy). Workers are not touched.",
        action: "Switch with summary",
        run: () => void call("orchestrator.use", plan.params),
        alt: {
          action: "Switch without summary",
          run: () =>
            void call("orchestrator.use", { ...plan.params, summary: false }),
        },
      });
      return;
    }
    if (plan.move) {
      confirm({
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
  const chooseRuntime = (r: string) => {
    setRuntime(r);
    setQuery("");
    setCustom(null);
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
    if (runsCommandLine(r)) return;
    if (!mobile()) search.current?.focus();
  };
  const chooseModel = (model: string | undefined) => {
    const pick = { ...pending, runtime, model };
    setPending(pick);
    setCustom(null);
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
    if (row.kind === "custom") setCustom(query.trim());
    else chooseModel(row.kind === "model" ? row.value : undefined);
  };
  const chooseEffort = (effort: string | null) =>
    apply({ ...pending, runtime, effort: effort ?? undefined });
  const done = () =>
    apply(
      locked ? { runtime } : { ...pending, runtime, effort: pending.effort },
    );

  const optionId = (column: string, i: number) => `${id}-${column}-${i}`;
  const active =
    nav.column === "rail"
      ? optionId("rail", nav.rail)
      : nav.column === "models"
        ? locked
          ? undefined
          : optionId("models", nav.model)
        : optionId("efforts", nav.effort);
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
            if (event.target === event.currentTarget) keyDown(event);
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
                className={
                  nav.column === "rail" && nav.rail === i ? "active" : ""
                }
                title={r === now.runtime ? `${r} (current)` : r}
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
            {locked ? (
              <p className="model-picker-note" id={`${id}-models`}>
                {target.name} runs its own command line: it has no model or
                effort. Pick another runtime to move it to an agent.
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
                        aria-label="Custom model id"
                        placeholder="Model id"
                        autoComplete="off"
                        spellCheck={false}
                        value={custom}
                        onChange={(event) => setCustom(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault();
                            event.stopPropagation();
                            if (custom.trim()) chooseModel(custom.trim());
                          }
                          if (event.key === "Escape") {
                            event.preventDefault();
                            event.stopPropagation();
                            setCustom(null);
                            search.current?.focus();
                          }
                        }}
                      />
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
                            : false
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
