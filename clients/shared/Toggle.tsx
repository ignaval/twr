import { useId, type ReactNode } from "react";

/** An on/off setting: its name and what it does on the left, a switch on the right. The
 * switch is a checkbox with `role="switch"`, named by the label and described by
 * `detail` (and `describedBy`, a note shown elsewhere in the row). */
export function Toggle({
  label,
  detail,
  checked,
  disabled,
  title,
  describedBy,
  children,
  onChange,
}: {
  label: string;
  detail?: string;
  checked: boolean;
  disabled?: boolean;
  /** A tooltip on the row (the config key it writes). */
  title?: string;
  describedBy?: string;
  /** Shown after the switch, in the row (a status). */
  children?: ReactNode;
  onChange(value: boolean): void;
}) {
  const id = useId();
  const described = [detail ? `${id}-detail` : "", describedBy || ""]
    .filter(Boolean)
    .join(" ");
  return (
    <div className="app-option" title={title}>
      <label htmlFor={id}>
        <span>{label}</span>
        {detail && <small id={`${id}-detail`}>{detail}</small>}
      </label>
      <input
        id={id}
        type="checkbox"
        role="switch"
        checked={checked}
        disabled={disabled}
        aria-describedby={described || undefined}
        onChange={(e) => onChange(e.target.checked)}
      />
      {children}
    </div>
  );
}
