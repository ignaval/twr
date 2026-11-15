// The colours the renderer actually draws, measured in the live window (scripts/omarchy-qa.mjs).
//
// `auditScreen` walks every element on screen that draws text, and every field, and reads
// what Chromium computed for it: the text colour, and the background under it as the stacked
// backgrounds and opacities of its ancestors composite. It returns each pair's WCAG ratio,
// whether the text is large (24 px, or 18.66 px bold), and whether it belongs to a disabled
// control (which WCAG exempts, and the app dims on purpose). Text another layer covers (a
// dialog's scrim over the page) or that is scrolled out of view is left out, as nobody reads it
// there. `focusRing` measures the ring of the element that has the keyboard focus.

/** Runs in the page. */
function measure({ screen, focusOnly }) {
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d", { willReadFrequently: true });
  const cache = new Map();
  /** Any CSS colour, as [r, g, b, a] with a in 0..1. */
  const rgba = (color) => {
    if (cache.has(color)) return cache.get(color);
    context.clearRect(0, 0, 1, 1);
    context.fillStyle = "#000";
    context.fillStyle = color;
    context.fillRect(0, 0, 1, 1);
    const [r, g, b, a] = context.getImageData(0, 0, 1, 1).data;
    const value = a === 0 ? [0, 0, 0, 0] : [r, g, b, a / 255];
    cache.set(color, value);
    return value;
  };
  const over = (top, bottom) => {
    const a = top[3];
    return [
      top[0] * a + bottom[0] * (1 - a),
      top[1] * a + bottom[1] * (1 - a),
      top[2] * a + bottom[2] * (1 - a),
      1,
    ];
  };
  const lum = (c) =>
    c
      .slice(0, 3)
      .map((v) => v / 255)
      .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
      .reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
  const ratio = (a, b) => {
    const [x, y] = [lum(a), lum(b)];
    return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
  };
  const hex = (c) =>
    "#" +
    c
      .slice(0, 3)
      .map((v) => Math.round(v).toString(16).padStart(2, "0"))
      .join("");
  const root = rgba(getComputedStyle(document.documentElement).backgroundColor);
  const page = root[3] ? root : [255, 255, 255, 1];
  /** The colour under `element`'s content: its own background over its ancestors', each
   * group's opacity applied. Also says whether a gradient or image was in the stack. */
  const backdrop = (element) => {
    const chain = [];
    for (
      let node = element;
      node && node.nodeType === 1;
      node = node.parentElement
    )
      chain.push(node);
    chain.reverse();
    let color = page;
    let opacity = 1;
    let image = false;
    for (const node of chain) {
      const style = getComputedStyle(node);
      opacity *= Number(style.opacity);
      if (style.backgroundImage !== "none") image = true;
      const background = rgba(style.backgroundColor);
      if (background[3])
        color = over(
          [background[0], background[1], background[2], background[3]],
          color,
        );
    }
    return { color, opacity, image };
  };
  const path = (element) => {
    const parts = [];
    for (
      let node = element;
      node && node !== document.body && parts.length < 4;
      node = node.parentElement
    ) {
      const classes = [...node.classList].slice(0, 2).join(".");
      parts.unshift(
        node.localName +
          (node.id && !/^:|^twr-|_r_|«/.test(node.id) ? "#" + node.id : "") +
          (classes ? "." + classes : ""),
      );
    }
    return parts.join(" > ");
  };
  // Inactive: disabled, or a request's options in the 600 ms before they answer.
  const disabled = (element) =>
    !!element.closest(
      ':disabled, [aria-disabled="true"], fieldset[disabled], .attention-options[data-armed="false"]',
    );
  const visibleAt = (element, rect) => {
    if (rect.width < 2 || rect.height < 2) return false;
    const x = Math.min(Math.max(rect.left + rect.width / 2, 0), innerWidth - 1);
    const y = Math.min(
      Math.max(rect.top + rect.height / 2, 0),
      innerHeight - 1,
    );
    if (
      rect.bottom <= 0 ||
      rect.right <= 0 ||
      rect.top >= innerHeight ||
      rect.left >= innerWidth
    )
      return false;
    const hit = document.elementFromPoint(x, y);
    return (
      !!hit &&
      (element === hit || element.contains(hit) || hit.contains(element))
    );
  };
  const text = [];
  const fields = [];
  const record = (element, kind, foreground, rect) => {
    const style = getComputedStyle(element);
    const under = backdrop(element);
    const fg = rgba(foreground);
    if (!fg[3]) return;
    const shown = over(
      [fg[0], fg[1], fg[2], fg[3] * under.opacity],
      under.color,
    );
    const size = parseFloat(style.fontSize);
    const bold = Number(style.fontWeight) >= 700;
    text.push({
      screen,
      kind,
      where: path(element),
      text: (kind === "text"
        ? element.textContent
        : element.value || element.placeholder || ""
      )
        .trim()
        .replace(/\s+/g, " ")
        .slice(0, 48),
      color: hex(shown),
      background: hex(under.color),
      ratio: Math.round(ratio(shown, under.color) * 100) / 100,
      large: size >= 24 || (size >= 18.66 && bold),
      disabled: disabled(element),
      threadName: !!element.closest(".thread-ink"),
      opacity: Math.round(under.opacity * 100) / 100,
      image: under.image,
      rect: [rect.left, rect.top, rect.width, rect.height].map(Math.round),
    });
  };
  if (!focusOnly) {
    const walker = document.createTreeWalker(
      document.body,
      NodeFilter.SHOW_TEXT,
    );
    const seen = new Set();
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      if (!node.textContent.trim()) continue;
      const element = node.parentElement;
      if (!element || seen.has(element)) continue;
      if (element.closest("svg, script, style, noscript, [hidden]")) continue;
      const style = getComputedStyle(element);
      if (style.visibility !== "visible" || style.display === "none") continue;
      const range = document.createRange();
      range.selectNodeContents(node);
      const rect = range.getBoundingClientRect();
      if (!visibleAt(element, rect)) continue;
      seen.add(element);
      record(element, "text", style.color, rect);
    }
    for (const element of document.querySelectorAll(
      'input:not([type="checkbox"], [type="radio"], [type="range"], [type="hidden"], [type="file"]), textarea, select, .composer',
    )) {
      const rect = element.getBoundingClientRect();
      if (!visibleAt(element, rect)) continue;
      const style = getComputedStyle(element);
      if (
        element.localName !== "div" &&
        (element.value || element.placeholder)
      ) {
        const placeholder = !element.value;
        record(
          element,
          placeholder ? "placeholder" : "value",
          placeholder
            ? getComputedStyle(element, "::placeholder").color
            : style.color,
          rect,
        );
      }
      const width = parseFloat(style.borderTopWidth);
      const edge = rgba(style.borderTopColor);
      if (!width || !edge[3] || style.borderTopStyle === "none") continue;
      const inside = backdrop(element);
      const around = backdrop(element.parentElement);
      const shown = over(edge, around.color);
      fields.push({
        screen,
        where: path(element),
        edge: hex(shown),
        inside: hex(inside.color),
        around: hex(around.color),
        ratio:
          Math.round(
            Math.min(ratio(shown, inside.color), ratio(shown, around.color)) *
              100,
          ) / 100,
        focused: element.matches(":focus-within"),
        disabled: disabled(element),
      });
    }
  }
  let focus = null;
  const active = document.activeElement;
  if (
    active &&
    active !== document.body &&
    active.matches(":focus-visible, :focus-within")
  ) {
    const style = getComputedStyle(active);
    const outline =
      style.outlineStyle !== "none" && parseFloat(style.outlineWidth) > 0
        ? rgba(style.outlineColor)
        : null;
    const ring =
      outline ||
      (style.boxShadow !== "none"
        ? rgba(
            style.boxShadow.match(
              /^(rgba?\([^)]*\)|color\([^)]*\)|#\w+)/,
            )?.[1] || "transparent",
          )
        : null) ||
      rgba(style.borderTopColor);
    const around = backdrop(active.parentElement);
    const inside = backdrop(active);
    const shown = over(ring, around.color);
    focus = {
      screen,
      where: path(active),
      text: (active.textContent || active.getAttribute("aria-label") || "")
        .trim()
        .slice(0, 40),
      visible: active.matches(":focus-visible"),
      kind: outline ? "outline" : "border",
      ring: hex(shown),
      around: hex(around.color),
      inside: hex(inside.color),
      ratio:
        Math.round(
          (outline
            ? ratio(shown, around.color)
            : Math.min(
                ratio(shown, around.color),
                ratio(shown, inside.color),
              )) * 100,
        ) / 100,
    };
  }
  return { text, fields, focus };
}

/** Every text and field pair on screen now, once transitions have ended and a request's
 * options on screen answer (they are dimmed for their first 600 ms). */
export async function auditScreen(page, screen) {
  await page
    .waitForFunction(
      () =>
        [...document.querySelectorAll(".attention-options")]
          .filter((n) => n.getClientRects().length)
          .every((n) => n.dataset.armed === "true"),
      null,
      { timeout: 5000 },
    )
    .catch(() => {});
  await page.evaluate(async () => {
    await new Promise((done) =>
      requestAnimationFrame(() => requestAnimationFrame(done)),
    );
    await Promise.all(
      document.getAnimations().map((a) => a.finished.catch(() => {})),
    );
  });
  return page.evaluate(measure, { screen, focusOnly: false });
}

/** The ring of the focused element (after a keyboard move, so :focus-visible applies). */
export async function focusRing(page, screen) {
  await page.evaluate(async () => {
    await new Promise((done) =>
      requestAnimationFrame(() => requestAnimationFrame(done)),
    );
    await Promise.all(
      document.getAnimations().map((a) => a.finished.catch(() => {})),
    );
  });
  return (await page.evaluate(measure, { screen, focusOnly: true })).focus;
}

/** The pairs that fall short: text (not of a disabled control) under 4.5:1, or 3:1 when
 * large; a field's edge or a focus ring under 3:1. */
export function shortfalls({ text, fields, focus }) {
  return [
    ...text
      .filter((t) => !t.disabled && t.ratio < (t.large ? 3 : 4.5))
      .map((t) => ({ type: "text", ...t })),
    ...fields
      .filter((f) => !f.disabled && f.ratio < 3)
      .map((f) => ({ type: "field", ...f })),
    ...focus
      .filter((f) => f && f.ratio < 3)
      .map((f) => ({ type: "focus", ...f })),
  ];
}
