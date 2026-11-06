import { useSyncExternalStore } from "react";
import { savedAppearance, selectAppearance } from "./appearance";

function subscribe(changed: () => void) {
  window.addEventListener("twr:appearance", changed);
  window.addEventListener("storage", changed);
  return () => {
    window.removeEventListener("twr:appearance", changed);
    window.removeEventListener("storage", changed);
  };
}

export function AppearanceOptions({
  labelledBy,
}: { labelledBy?: string } = {}) {
  const theme = useSyncExternalStore(subscribe, savedAppearance);
  return (
    <div
      className="segmented"
      role="group"
      aria-label={labelledBy ? undefined : "Appearance"}
      aria-labelledby={labelledBy}
    >
      {(["dark", "light", "system"] as const).map((value) => (
        <button
          type="button"
          key={value}
          className={theme === value ? "active" : ""}
          aria-pressed={theme === value}
          onClick={() => selectAppearance(value)}
        >
          {value[0].toUpperCase() + value.slice(1)}
        </button>
      ))}
    </div>
  );
}
