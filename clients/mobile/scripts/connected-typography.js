// AsyncFunction body executed by native QA after pairing and opening a real PTY.
const root = document.documentElement;
const saved = root.getAttribute("style");
const frame = () =>
  new Promise((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(resolve)),
  );
const settle = async () => {
  // Expanding mounts the terminal and schedules a focus RAF. Let that commit
  // before dismissing the IME so the focus cannot reopen it after hide().
  await frame();
  await new Promise((resolve) => setTimeout(resolve, 140));
  document.activeElement?.blur?.();
  await Promise.resolve(window.Capacitor?.Plugins.Keyboard?.hide?.()).catch(
    () => {},
  );
  let previous = "",
    stable = 0;
  for (let i = 0; i < 50; i++) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    await frame();
    const current = `${visualViewport?.height}:${root.style.getPropertyValue("--viewport-height")}`;
    stable =
      root.dataset.keyboard !== "true" && current === previous ? stable + 1 : 0;
    if (stable >= 3) return;
    previous = current;
  }
  throw new Error(
    `IME did not settle closed: ${JSON.stringify({ keyboard: root.dataset.keyboard, inner: innerHeight, visual: visualViewport?.height, style: root.getAttribute("style") })}`,
  );
};
let restoreExpanded = false;
const measurements = [];
try {
  const tabs = document.querySelector(".terminal-tabs");
  const dock = document.querySelector(".terminal-dock");
  if (!tabs || !dock)
    throw new Error("Connected typography QA requires a visible terminal");
  const checkTabs = () => {
    const bar = tabs.getBoundingClientRect(),
      pane = dock.getBoundingClientRect();
    for (const button of tabs.querySelectorAll("button")) {
      const box = button.getBoundingClientRect();
      if (
        box.top < bar.top - 1 ||
        box.bottom > bar.bottom + 1 ||
        box.bottom > pane.bottom + 1
      )
        throw new Error(
          "Large terminal controls are clipped: " +
            (button.getAttribute("aria-label") || button.textContent),
        );
    }
  };
  for (const size of [26, 28.3, 44]) {
    const code = Math.min(size, 32);
    root.style.setProperty("--text-size", size + "px");
    root.style.setProperty("--reading-size", size + "px");
    root.style.setProperty("--code-size", code + "px");
    window.dispatchEvent(new Event("twr:typography"));
    await settle();
    checkTabs();
    const surface =
      document.querySelector(
        '.terminal-session[style*="display: flex"] .terminal-surface',
      ) ||
      [...document.querySelectorAll(".terminal-surface")].find(
        (node) => node.getBoundingClientRect().height > 0,
      );
    const output = surface?.getBoundingClientRect();
    const rootFont = parseFloat(getComputedStyle(root).fontSize);
    if (Math.abs(rootFont - size) > 0.05)
      throw new Error(
        `Requested ${size}px but native root computed ${rootFont}px`,
      );
    const geometry = () => ({
      size,
      code,
      rootFont: parseFloat(getComputedStyle(root).fontSize),
      keyboard: root.dataset.keyboard,
      viewport: {
        inner: innerHeight,
        visual: visualViewport?.height,
        css: getComputedStyle(root).getPropertyValue("--viewport-height"),
      },
      dock: dock.getBoundingClientRect().toJSON(),
      surface: surface?.getBoundingClientRect().toJSON(),
      renderedRows: surface?.querySelector(".xterm-rows")?.childElementCount,
      tabs: tabs.getBoundingClientRect().toJSON(),
      search: surface?.parentElement
        .querySelector(".terminal-search")
        ?.getBoundingClientRect()
        .toJSON(),
      accessory: surface?.parentElement
        .querySelector(".terminal-accessory")
        ?.getBoundingClientRect()
        .toJSON(),
      workArea: getComputedStyle(document.querySelector(".work-area")).display,
    });
    const closedGeometry = geometry();
    const rows = output ? Math.floor((output.height - 12) / (code * 1.25)) : 0;
    if (rows < 2 || closedGeometry.renderedRows < 2)
      throw new Error(
        `Only ${rows} terminal rows remain: ${JSON.stringify({ ...closedGeometry, measurements })}`,
      );
    let keyboardGeometry;
    if (size === 44 && window.Capacitor?.getPlatform?.() === "android") {
      surface.querySelector(".xterm-helper-textarea").focus();
      await window.Capacitor.Plugins.Keyboard.show();
      for (let i = 0; i < 50 && root.dataset.keyboard !== "true"; i++)
        await new Promise((resolve) => setTimeout(resolve, 100));
      await new Promise((resolve) => setTimeout(resolve, 400));
      await frame();
      keyboardGeometry = geometry();
      if (Math.abs(keyboardGeometry.rootFont - size) > 0.05)
        throw new Error(
          `Native IME changed root font: ${JSON.stringify(keyboardGeometry)}`,
        );
      if (keyboardGeometry.keyboard !== "true")
        throw new Error(
          `Native keyboard did not open: ${JSON.stringify(keyboardGeometry)}`,
        );
      const keyboardRows = Math.floor(
        (keyboardGeometry.surface.height - 12) / (code * 1.25),
      );
      if (keyboardRows < 2 || keyboardGeometry.renderedRows < 2)
        throw new Error(
          `Only ${keyboardRows} terminal rows with native IME: ${JSON.stringify(keyboardGeometry)}`,
        );
      keyboardGeometry.rows = keyboardRows;
      checkTabs();
      await settle();
    }
    const nav = document.querySelector(".mobile-tabs");
    if (nav?.getBoundingClientRect().height) {
      const bar = nav.getBoundingClientRect();
      for (const label of nav.querySelectorAll("button > span")) {
        const box = label.getBoundingClientRect();
        if (
          box.top < bar.top - 1 ||
          box.bottom > bar.bottom + 1 ||
          box.width > label.parentElement.clientWidth + 1
        )
          throw new Error(
            "Large navigation label overflows: " + label.textContent,
          );
      }
    }
    document.querySelector('[aria-label="Collapse terminals"]')?.click();
    restoreExpanded = true;
    await settle();
    checkTabs();
    const input = document.querySelector(".composer textarea");
    if (!input)
      throw new Error(
        "Connected typography QA requires the conversation composer",
      );
    const box = input.getBoundingClientRect(),
      style = getComputedStyle(input);
    const font = parseFloat(style.fontSize),
      line = parseFloat(style.lineHeight);
    if (box.height < 43.5 || box.bottom > dock.getBoundingClientRect().top + 1)
      throw new Error("Large composer is clipped above collapsed terminals");
    if (line < font * 1.2) throw new Error("Large composer lines overlap");
    if (document.body.scrollWidth > innerWidth + 1)
      throw new Error("Connected large text overflows viewport");
    measurements.push({
      size,
      code,
      rootFont,
      keyboard: root.dataset.keyboard,
      viewport: { inner: innerHeight, visual: visualViewport?.height },
      rows,
      renderedRows: closedGeometry.renderedRows,
      ...(keyboardGeometry && { keyboardGeometry }),
      terminal: output.toJSON(),
      composer: { font, line, height: box.height },
    });
    document.querySelector('[aria-label="Expand terminals"]')?.click();
    restoreExpanded = false;
  }
  return { pass: true, width: innerWidth, measurements };
} finally {
  if (restoreExpanded)
    document.querySelector('[aria-label="Expand terminals"]')?.click();
  if (saved === null) root.removeAttribute("style");
  else root.setAttribute("style", saved);
  window.dispatchEvent(new Event("twr:typography"));
  await frame();
}
