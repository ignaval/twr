import { useEffect, useRef, useState, type PointerEvent } from "react";
import {
  defaultLayout,
  initializeLayout,
  updateLayout,
  useLayoutPreferences,
} from "./layoutPreferences";

export function LayoutOptions() {
  const prefs = useLayoutPreferences();
  return (
    <section className="settings-section layout-options">
      <h2>Layout & text</h2>
      <p>
        Pane widths are remembered on this device. Mobile text also follows your
        system text size. Sizes below are before system scaling; interface and
        terminal controls use a bounded scale while conversation text follows
        the full setting.
      </p>
      <label>
        Density
        <select
          value={prefs.density}
          onChange={(e) =>
            updateLayout({
              density: e.target.value as "compact" | "comfortable",
            })
          }
        >
          <option value="comfortable">Comfortable</option>
          <option value="compact">Compact</option>
        </select>
      </label>
      <label>
        Interface text <output>{prefs.textSize} px</output>
        <input
          type="range"
          aria-label="Interface text"
          min="12"
          max="22"
          aria-valuetext={`${prefs.textSize} pixels before system scaling`}
          value={prefs.textSize}
          onChange={(e) => updateLayout({ textSize: Number(e.target.value) })}
        />
      </label>
      <label>
        Code & terminal text <output>{prefs.codeSize} px</output>
        <input
          type="range"
          aria-label="Code & terminal text"
          min="11"
          max="24"
          aria-valuetext={`${prefs.codeSize} pixels before system scaling`}
          value={prefs.codeSize}
          onChange={(e) => updateLayout({ codeSize: Number(e.target.value) })}
        />
      </label>
      <div className="pane-width-controls">
        {(["sidebar", "inspector"] as const).map((pane) => (
          <label key={pane}>
            {pane === "sidebar" ? "Sidebar width" : "Inspector width"}
            <output>{prefs[pane]} px</output>
            <input
              type="range"
              aria-label={
                pane === "sidebar" ? "Sidebar width" : "Inspector width"
              }
              min={pane === "sidebar" ? 200 : 280}
              max={pane === "sidebar" ? 440 : 600}
              value={prefs[pane]}
              aria-valuetext={`${prefs[pane]} pixels, constrained to fit the window`}
              onChange={(event) =>
                updateLayout({ [pane]: Number(event.target.value) })
              }
            />
          </label>
        ))}
      </div>
      <button onClick={() => updateLayout(defaultLayout)}>
        Reset layout & text
      </button>
    </section>
  );
}

export function PaneResize({ pane }: { pane: "sidebar" | "inspector" }) {
  const prefs = useLayoutPreferences();
  const handle = useRef<HTMLDivElement>(null);
  const [rendered, setRendered] = useState(prefs[pane]);
  const [maximum, setMaximum] = useState(pane === "sidebar" ? 440 : 600);
  const drag = useRef<{
    x: number;
    width: number;
    pointer: number;
    latest: number;
    oldWidth: string;
    oldMin: string;
  } | null>(null);
  const minimum = pane === "sidebar" ? 200 : 280;
  useEffect(() => {
    initializeLayout();
    const node = handle.current?.parentElement;
    if (!node) return;
    const measure = () => {
      const width = node.getBoundingClientRect().width;
      if (width) setRendered(Math.round(width));
      const available =
        node.parentElement?.getBoundingClientRect().width || innerWidth;
      const max =
        pane === "sidebar"
          ? Math.min(440, innerWidth * (innerWidth <= 1100 ? 0.34 : 0.38))
          : Math.min(600, available * (innerWidth <= 1100 ? 0.9 : 0.38));
      setMaximum(Math.max(minimum, Math.floor(max)));
    };
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    if (node.parentElement) observer.observe(node.parentElement);
    window.addEventListener("resize", measure);
    measure();
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [pane]);
  const finish = () => {
    const node = handle.current?.parentElement;
    const previous = drag.current;
    drag.current = null;
    if (node && previous) {
      node.style.width = previous.oldWidth;
      node.style.minWidth = previous.oldMin;
      if (Math.abs(previous.latest - previous.width) >= 1)
        updateLayout({ [pane]: previous.latest });
    }
    document.documentElement.classList.remove("resizing-pane");
  };
  useEffect(() => finish, []);
  const move = (event: PointerEvent<HTMLDivElement>) => {
    const previous = drag.current;
    const node = handle.current?.parentElement;
    if (!previous || !node || previous.pointer !== event.pointerId) return;
    const value = Math.max(
      minimum,
      Math.min(
        maximum,
        previous.width +
          (event.clientX - previous.x) * (pane === "sidebar" ? 1 : -1),
      ),
    );
    previous.latest = value;
    // Preview on the pane itself: no disk writes or terminal theme updates during a drag.
    node.style.width = `${value}px`;
    node.style.minWidth = `${value}px`;
  };
  return (
    <div
      ref={handle}
      role="separator"
      tabIndex={0}
      aria-label={`Resize ${pane}`}
      aria-controls={`twr-${pane}`}
      aria-orientation="vertical"
      aria-valuemin={minimum}
      aria-valuemax={maximum}
      aria-valuenow={Math.min(maximum, Math.max(minimum, rendered))}
      aria-valuetext={`${rendered} pixels`}
      className={`pane-resize pane-resize-${pane}`}
      title="Drag to resize, use arrow keys, or change widths in Settings"
      onPointerDown={(event) => {
        const node = event.currentTarget.parentElement;
        if (event.button !== 0 || !node) return;
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        const width = node.getBoundingClientRect().width;
        drag.current = {
          x: event.clientX,
          width,
          pointer: event.pointerId,
          latest: width,
          oldWidth: node.style.width,
          oldMin: node.style.minWidth,
        };
        document.documentElement.classList.add("resizing-pane");
      }}
      onPointerMove={move}
      onPointerUp={finish}
      onPointerCancel={finish}
      onLostPointerCapture={finish}
      onDoubleClick={() => updateLayout({ [pane]: defaultLayout[pane] })}
      onKeyDown={(event) => {
        if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key))
          return;
        event.preventDefault();
        const width =
          handle.current?.parentElement?.getBoundingClientRect().width ||
          rendered;
        const value =
          event.key === "Home"
            ? minimum
            : event.key === "End"
              ? maximum
              : width +
                (event.key === "ArrowRight" ? 10 : -10) *
                  (pane === "sidebar" ? 1 : -1);
        const clamped = Math.max(minimum, Math.min(maximum, value));
        if (event.key.startsWith("Arrow") && Math.abs(clamped - width) < 1)
          return;
        updateLayout({ [pane]: clamped });
      }}
    />
  );
}
