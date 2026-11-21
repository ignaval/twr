import { useEffect, useId, useRef, useState } from "react";
import { MoreHorizontal } from "lucide-react";
import { handleRovingKey } from "./a11y";

export interface MenuItem {
  label: string;
  onSelect: () => void;
  danger?: boolean;
}

/** A "⋯" button that opens a short menu of more actions (`role="menu"`): arrow keys,
 * Home and End move between items, Escape or Tab closes it, and focus goes back to the
 * button after a choice. `label` names the button ("More actions for devbox"). */
export function MenuButton({
  label,
  items,
}: {
  label: string;
  items: MenuItem[];
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const focusFirst = useRef<"first" | "last">("first");
  useEffect(() => {
    if (!open) return;
    const items =
      menu.current?.querySelectorAll<HTMLElement>("[role=menuitem]");
    (focusFirst.current === "last"
      ? items?.[items.length - 1]
      : items?.[0]
    )?.focus();
    const close = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (!menu.current?.contains(target) && !button.current?.contains(target))
        setOpen(false);
    };
    document.addEventListener("pointerdown", close);
    return () => document.removeEventListener("pointerdown", close);
  }, [open]);
  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) button.current?.focus();
  };
  return (
    <div className="menu-button">
      <button
        ref={button}
        type="button"
        className="icon-button"
        aria-label={label}
        title={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        onClick={() => {
          focusFirst.current = "first";
          setOpen(!open);
        }}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            focusFirst.current = event.key === "ArrowUp" ? "last" : "first";
            setOpen(true);
          }
        }}
      >
        <MoreHorizontal size={16} aria-hidden="true" />
      </button>
      {open && (
        <div
          ref={menu}
          id={id}
          role="menu"
          aria-label={label}
          className="menu-popup"
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              event.preventDefault();
              event.stopPropagation();
              close(true);
            } else if (event.key === "Tab") close(false);
            else
              handleRovingKey(
                event,
                menu.current,
                "[role=menuitem]",
                "vertical",
              );
          }}
        >
          {items.map((item) => (
            <button
              key={item.label}
              type="button"
              role="menuitem"
              tabIndex={-1}
              className={item.danger ? "danger-quiet" : undefined}
              onClick={() => {
                close(true);
                item.onSelect();
              }}
            >
              {item.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
