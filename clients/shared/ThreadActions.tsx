import { useEffect, useState } from "react";
import { ChevronDown, Pencil, Pin, Play, Square } from "lucide-react";
import { ActionsMenu, type MenuItem } from "./ActionsMenu";
import { permissionChoices, permissionLabel } from "./labels";
import type { Json, OperationOptions, Thread } from "./types";
import "./inspector.css";

/** Statuses with a turn or background work going: Stop interrupts them. */
const STOPPABLE = ["running", "background", "waiting"];

/**
 * The thread inspector's actions: the one that fits the thread's state (**Stop…** while it
 * works, **Resume…** when parked: the hub resumes only a parked thread), **Pin** and
 * **Rename…**, and **More** with the less common ones, each with a line saying what it does.
 */
export function ThreadActions({
  thread,
  has,
  pinned,
  onPin,
  action,
  more,
}: {
  thread: Thread;
  /** Whether this device may run a command (by id). */
  has: (id: string) => boolean;
  pinned: boolean;
  onPin: () => void;
  /** Opens an action's form for this thread (`fixed`: the form doesn't ask which). */
  action: (id: string, initial?: Json, options?: OperationOptions) => void;
  more: MenuItem[];
}) {
  const stoppable = STOPPABLE.includes(thread.status) && has("stop");
  const resumable = thread.status === "parked" && has("resume");
  return (
    <div className="inspector-actions" role="group" aria-label="Thread actions">
      {stoppable && (
        <button
          aria-haspopup="dialog"
          onClick={() => action("stop", { thread: thread.id }, { fixed: true })}
        >
          <Square size={14} aria-hidden="true" />
          Stop…
        </button>
      )}
      {resumable && (
        <button
          aria-haspopup="dialog"
          onClick={() =>
            action("resume", { thread: thread.id }, { fixed: true })
          }
        >
          <Play size={14} aria-hidden="true" />
          Resume…
        </button>
      )}
      <button onClick={onPin}>
        <Pin size={14} aria-hidden="true" />
        {pinned ? "Unpin" : "Pin"}
      </button>
      {has("rename") && (
        <button
          aria-haspopup="dialog"
          onClick={() =>
            action("rename", { thread: thread.id }, { fixed: true })
          }
        >
          <Pencil size={14} aria-hidden="true" />
          Rename…
        </button>
      )}
      <ActionsMenu
        inline
        className="inspector-more"
        label={`More actions for ${thread.name}`}
        text="More"
        icon={<ChevronDown size={14} aria-hidden="true" />}
        items={more}
      />
    </div>
  );
}

/**
 * The details table's permission mode: a dropdown of the modes in words (narrowed without
 * `admin`, as the hub would refuse wider ones; with it, the runtime's own modes too) that
 * changes the thread's mode at once (`thread.set_mode`). Without the right to change it, and
 * for the orchestrator, whose mode the hub fixes, the mode in words.
 */
export function PermissionModePicker({
  id,
  thread,
  admin,
  editable,
  rpc,
  fail,
  onChanged,
}: {
  id: string;
  thread: Thread;
  admin: boolean;
  editable: boolean;
  rpc: (method: string, params?: Json) => Promise<Json>;
  fail: (error: unknown) => void;
  onChanged: (label: string) => void;
}) {
  // The mode asked for, shown until the hub's view says the thread has it (or it fails).
  const [pending, setPending] = useState<string>();
  const current: string = thread.mode || "";
  useEffect(() => setPending(undefined), [current, thread.id]);
  if (!editable || thread.is_orchestrator)
    return (
      <b
        id={id}
        className="inspector-mode"
        title={
          thread.is_orchestrator
            ? "The hub fixes the orchestrator's mode so that it only asks."
            : undefined
        }
      >
        {permissionLabel(current) || "—"}
      </b>
    );
  const choices = permissionChoices(current, admin, thread.runtime);
  const value = pending ?? current;
  const chosen = choices.find((c) => c.value === value);
  return (
    <select
      id={id}
      className="inspector-mode"
      value={value}
      title={chosen?.description}
      disabled={pending !== undefined}
      aria-busy={pending !== undefined || undefined}
      onChange={(event) => {
        const mode = event.target.value;
        if (!mode || mode === current) return;
        setPending(mode);
        void rpc("thread.set_mode", { thread: thread.id, mode })
          .then(() => {
            onChanged(permissionLabel(mode));
            // The view normally brings the new mode first; never hold the picker.
            setTimeout(() => setPending(undefined), 4000);
          })
          .catch((error) => {
            setPending(undefined);
            fail(error);
          });
      }}
    >
      {!value && <option value="">—</option>}
      {choices.map((choice) => (
        <option key={choice.value} value={choice.value}>
          {permissionLabel(choice.value)}
        </option>
      ))}
    </select>
  );
}
