import type { ReactNode } from "react";
import { MoreHorizontal } from "lucide-react";
import { handleRovingKey } from "./a11y";
import "./actions-menu.css";

/** One row of an actions menu. `pressed` makes it a toggle (a check shows when on);
 * `hint` is a one-line explanation under the label. */
export interface MenuItem {
  key: string;
  label: string;
  icon?: ReactNode;
  onSelect: () => void;
  pressed?: boolean;
  disabled?: boolean;
  hint?: string;
  danger?: boolean;
  /** Opens a dialog (the label ends in "…"). */
  dialog?: boolean;
  /** Starts a group: a rule above it. */
  group?: boolean;
  className?: string;
}

/**
 * A "⋯" (or labelled) disclosure that holds less common actions. Keyboard: Enter, Space or
 * ↓ on the button opens it and focuses the first action; ↑/↓, Home and End move between
 * actions; Escape closes it and returns to the button (App's window handler), as does a
 * pointer press outside it. Choosing an action gives focus back to the button and closes
 * it before the action runs, so a dialog the action opens returns focus there. `inline`
 * lays the actions out in the flow under the button (the inspector) instead of over the
 * page.
 */
export function ActionsMenu({
  label,
  items,
  className = "",
  text,
  icon,
  inline = false,
}: {
  label: string;
  items: MenuItem[];
  className?: string;
  /** Visible words beside the icon (otherwise the button shows the icon alone). */
  text?: string;
  icon?: ReactNode;
  inline?: boolean;
}) {
  if (!items.length) return null;
  return (
    <details
      className={`actions-menu ${inline ? "inline" : ""} ${className}`}
      onToggle={(event) => {
        const menu = event.currentTarget;
        // Keyboard users land on the first action; pointer users keep focus.
        if (menu.open && menu.dataset.keyboard === "true")
          menu
            .querySelector<HTMLElement>(
              ".actions-menu-items > button:not(:disabled)",
            )
            ?.focus();
        delete menu.dataset.keyboard;
      }}
    >
      <summary
        className={`no-drag ${text ? "actions-menu-labelled" : ""}`}
        aria-label={text ? undefined : label}
        title={label}
        onKeyDown={(event) => {
          const menu = event.currentTarget.parentElement as HTMLDetailsElement;
          if (["Enter", " "].includes(event.key))
            menu.dataset.keyboard = "true";
          if (event.key === "ArrowDown") {
            event.preventDefault();
            menu.dataset.keyboard = "true";
            if (!menu.open) menu.open = true;
            else
              menu
                .querySelector<HTMLElement>(
                  ".actions-menu-items > button:not(:disabled)",
                )
                ?.focus();
          }
        }}
      >
        {icon ?? <MoreHorizontal size={18} aria-hidden="true" />}
        {text}
      </summary>
      <div
        className="actions-menu-items no-drag"
        onKeyDown={(event) =>
          handleRovingKey(event, event.currentTarget, ":scope > button")
        }
      >
        {items.map((item) => (
          <button
            type="button"
            key={item.key}
            className={[
              item.danger ? "danger-quiet" : "",
              item.group ? "actions-menu-group" : "",
              item.className || "",
            ]
              .filter(Boolean)
              .join(" ")}
            aria-pressed={item.pressed}
            aria-haspopup={item.dialog ? "dialog" : undefined}
            disabled={item.disabled}
            onClick={(event) => {
              // Focus leaves the action before the menu hides it: on the button, which a
              // dialog the action opens then gives focus back to.
              const menu = event.currentTarget.closest("details");
              menu?.querySelector("summary")?.focus();
              menu?.removeAttribute("open");
              item.onSelect();
            }}
          >
            {item.icon}
            {item.hint ? (
              <span className="actions-menu-text">
                <span>{item.label}</span>
                <small>{item.hint}</small>
              </span>
            ) : (
              item.label
            )}
          </button>
        ))}
      </div>
    </details>
  );
}
