import { useState, type ReactNode } from "react";
import "./status-strip.css";

/** One notice for the status strip under the header. */
export interface StripItem {
  key: string;
  /** The live region it is: `alert` for a problem that needs the user now, `status` for
   * news. None for a notice that is a panel of its own (the assistance disclosure). */
  role?: "status" | "alert";
  tone?: "warning" | "offline";
  busy?: boolean;
  icon?: ReactNode;
  content: ReactNode;
  /** Its buttons (Reconnect, Restart to update, Dismiss…). */
  actions?: ReactNode;
}

/**
 * The app's notices, one at a time: the first of `items` (the caller ranks them, the most
 * pressing first), with **+N** to show the others under it. Dismissing the one shown brings
 * up the next.
 */
export function StatusStrip({ items }: { items: StripItem[] }) {
  const [expanded, setExpanded] = useState(false);
  if (!items.length) return null;
  const more = items.length - 1;
  const shown = expanded && more ? items : items.slice(0, 1);
  return (
    <div className="status-strip">
      {shown.map((item, index) => (
        <div
          key={item.key}
          className={`status-strip-item ${item.tone || ""}`}
          data-notice={item.key}
          role={item.role}
          aria-busy={item.busy || undefined}
        >
          {item.icon}
          <div className="status-strip-content">{item.content}</div>
          {(item.actions || (index === 0 && more > 0)) && (
            <div className="status-strip-actions">
              {item.actions}
              {index === 0 && more > 0 && (
                <button
                  type="button"
                  className="status-strip-more"
                  aria-expanded={expanded}
                  aria-label={
                    expanded
                      ? "Show one notice"
                      : `Show ${more} more ${more === 1 ? "notice" : "notices"}`
                  }
                  title={expanded ? "Show one notice" : "More notices"}
                  onClick={() => setExpanded((v) => !v)}
                >
                  {expanded ? "Less" : `+${more}`}
                </button>
              )}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
