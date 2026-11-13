import { useEffect, useRef, type RefObject } from "react";

/* Shared accessibility helpers: one screen-reader announcer, modal focus
 * management, roving arrow-key navigation, and motion preferences. The pure
 * functions are unit tested; the DOM helpers keep mouse and touch behavior
 * unchanged. */

export type Politeness = "polite" | "assertive";

interface AnnouncerOptions {
  /** Delay before a queued message is written, so bursts are coalesced. */
  delay?: number;
  /** Drop an identical message repeated within this window. */
  repeatWindow?: number;
  /** Clear spoken text after this long so a virtual cursor does not find stale text. */
  clearAfter?: number;
  schedule?: (callback: () => void, ms: number) => unknown;
  cancel?: (handle: unknown) => void;
  now?: () => number;
}

/** Elements carrying this attribute stay reachable while a modal is open. */
export const MODAL_KEEP = "data-modal-keep";

export function createAnnouncer(
  doc: Pick<Document, "createElement" | "body">,
  {
    delay = 150,
    repeatWindow = 1500,
    clearAfter = 6000,
    schedule = (callback, ms) => setTimeout(callback, ms),
    cancel = (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
    now = () => Date.now(),
  }: AnnouncerOptions = {},
) {
  const regions: Partial<Record<Politeness, HTMLElement>> = {};
  const pending: Record<Politeness, string[]> = { polite: [], assertive: [] };
  const flushTimer: Partial<Record<Politeness, unknown>> = {};
  const clearTimer: Partial<Record<Politeness, unknown>> = {};
  const recent = new Map<string, number>();
  function region(politeness: Politeness) {
    let node = regions[politeness];
    if (!node || !node.isConnected) {
      node = doc.createElement("div");
      node.setAttribute("aria-live", politeness);
      node.setAttribute("aria-atomic", "true");
      node.setAttribute("data-twr-live", politeness);
      node.setAttribute(MODAL_KEEP, "");
      node.className = "visually-hidden";
      node.setAttribute(
        "style",
        "position:absolute;width:1px;height:1px;margin:-1px;padding:0;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap;border:0",
      );
      doc.body.appendChild(node);
      regions[politeness] = node;
    }
    return node;
  }
  function flush(politeness: Politeness) {
    flushTimer[politeness] = undefined;
    const messages = pending[politeness].splice(0);
    if (!messages.length) return;
    const node = region(politeness);
    node.textContent = messages.join(". ");
    if (clearTimer[politeness] !== undefined) cancel(clearTimer[politeness]);
    clearTimer[politeness] = schedule(() => {
      clearTimer[politeness] = undefined;
      node.textContent = "";
    }, clearAfter);
  }
  return {
    /** Ensure both regions exist before the first message (some readers ignore new regions). */
    prepare() {
      region("polite");
      region("assertive");
    },
    announce(message: string, politeness: Politeness = "polite") {
      const text = message.replace(/\s+/g, " ").trim();
      if (!text) return;
      const at = now();
      const key = politeness + ":" + text;
      const last = recent.get(key);
      if (last !== undefined && at - last < repeatWindow) return;
      recent.set(key, at);
      if (recent.size > 50)
        for (const [old, when] of recent)
          if (at - when >= repeatWindow) recent.delete(old);
      if (pending[politeness].includes(text)) return;
      pending[politeness].push(text);
      // Clear first so the same text is spoken again when it legitimately repeats.
      region(politeness).textContent = "";
      if (flushTimer[politeness] === undefined)
        flushTimer[politeness] = schedule(() => flush(politeness), delay);
    },
  };
}

let shared: ReturnType<typeof createAnnouncer> | undefined;
function sharedAnnouncer() {
  if (typeof document === "undefined" || !document.body) return undefined;
  shared ??= createAnnouncer(document);
  return shared;
}
/** Speak a short message through the shared live regions. */
export function announce(message: string, politeness: Politeness = "polite") {
  sharedAnnouncer()?.announce(message, politeness);
}
export function prepareAnnouncer() {
  sharedAnnouncer()?.prepare();
}

export const FOCUSABLE = [
  "a[href]",
  "area[href]",
  "button:not(:disabled)",
  "input:not(:disabled):not([type=hidden])",
  "select:not(:disabled)",
  "textarea:not(:disabled)",
  "summary",
  "iframe",
  "[contenteditable]:not([contenteditable=false])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

/** Keyboard-reachable elements inside a root, in DOM order. */
export function focusableWithin(root: ParentNode | null | undefined) {
  if (!root) return [];
  return [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(
    (node) =>
      node.getClientRects().length > 0 &&
      !node.closest("[inert]") &&
      node.tabIndex >= 0,
  );
}

/** Where Tab should wrap to inside a trap, or undefined to let the browser move focus. */
export function trapTarget(
  count: number,
  index: number,
  backwards: boolean,
): number | undefined {
  if (count <= 0) return undefined;
  if (index < 0) return backwards ? count - 1 : 0;
  if (backwards && index === 0) return count - 1;
  if (!backwards && index === count - 1) return 0;
  return undefined;
}

export type Orientation = "vertical" | "horizontal" | "both";
/** Next index for arrow/Home/End keys in a menu, tab list or listbox. */
export function rovingIndex(
  key: string,
  index: number,
  count: number,
  orientation: Orientation = "vertical",
): number | undefined {
  if (count <= 0) return undefined;
  const next =
    (orientation !== "horizontal" && key === "ArrowDown") ||
    (orientation !== "vertical" && key === "ArrowRight");
  const previous =
    (orientation !== "horizontal" && key === "ArrowUp") ||
    (orientation !== "vertical" && key === "ArrowLeft");
  if (key === "Home") return 0;
  if (key === "End") return count - 1;
  if (next) return index < 0 ? 0 : (index + 1) % count;
  if (previous) return index < 0 ? count - 1 : (index - 1 + count) % count;
  return undefined;
}

/** Move focus between `selector` items of `container` for arrow, Home and End keys. */
export function handleRovingKey(
  event: {
    key: string;
    preventDefault(): void;
    altKey?: boolean;
    ctrlKey?: boolean;
    metaKey?: boolean;
  },
  container: HTMLElement | null,
  selector: string,
  orientation: Orientation = "vertical",
) {
  if (!container || event.altKey || event.ctrlKey || event.metaKey) return;
  const items = [...container.querySelectorAll<HTMLElement>(selector)].filter(
    (item) =>
      !(item as HTMLButtonElement).disabled && item.getClientRects().length,
  );
  const index = items.indexOf(document.activeElement as HTMLElement);
  const next = rovingIndex(event.key, index, items.length, orientation);
  if (next === undefined) return;
  event.preventDefault();
  items[next].focus();
  return items[next];
}

/** Hide everything outside `node` from assistive technology and pointer/keyboard input. */
export function inertOutside(node: HTMLElement) {
  const changed: HTMLElement[] = [];
  let current: HTMLElement | null = node;
  while (current?.parentElement && current !== document.body) {
    for (const sibling of current.parentElement.children) {
      if (sibling === current || !(sibling instanceof HTMLElement)) continue;
      if (
        sibling.inert ||
        sibling.matches(
          `[${MODAL_KEEP}],[role="alert"],[aria-live],script,style,template`,
        )
      )
        continue;
      sibling.inert = true;
      changed.push(sibling);
    }
    current = current.parentElement;
  }
  return () => {
    for (const element of changed) element.inert = false;
  };
}

const modals: HTMLElement[] = [];
// Recent focus targets, so a modal whose field autofocuses during render can
// still return focus to the control that opened it.
const focusHistory: HTMLElement[] = [];
if (typeof document !== "undefined")
  document.addEventListener(
    "focusin",
    (event) => {
      if (!(event.target instanceof HTMLElement)) return;
      focusHistory.push(event.target);
      if (focusHistory.length > 8) focusHistory.shift();
    },
    true,
  );
function opener(root: HTMLElement) {
  const active = document.activeElement;
  if (active instanceof HTMLElement && active !== document.body)
    if (!root.contains(active)) return active;
  for (let i = focusHistory.length - 1; i >= 0; i--) {
    const node = focusHistory[i];
    if (node.isConnected && !root.contains(node)) return node;
  }
  return undefined;
}

/** Modal behavior for dialogs and sheets: initial focus, Tab trap, Escape,
 * background inertness and focus restoration to the opener. */
export function useModal(
  ref: RefObject<HTMLElement | null>,
  {
    active = true,
    onClose,
    initialFocus,
    inertBackground = true,
    container,
  }: {
    active?: boolean;
    onClose: () => void;
    initialFocus?: (root: HTMLElement) => HTMLElement | null | undefined;
    inertBackground?: boolean;
    /** The outermost element of the modal (for example its scrim). */
    container?: (root: HTMLElement) => HTMLElement | null | undefined;
  },
) {
  const close = useRef(onClose);
  close.current = onClose;
  const focusFirst = useRef(initialFocus);
  focusFirst.current = initialFocus;
  useEffect(() => {
    const root = ref.current;
    if (!active || !root) return;
    const previous = opener(root);
    modals.push(root);
    const restoreInert = inertBackground
      ? inertOutside(container?.(root) || root)
      : undefined;
    const first = () => {
      // An autofocused field inside the modal already has focus; keep it.
      if (root.contains(document.activeElement)) return;
      const target = focusFirst.current?.(root) || focusableWithin(root)[0];
      if (target) target.focus();
      else {
        if (!root.hasAttribute("tabindex")) root.tabIndex = -1;
        root.focus();
      }
    };
    first();
    const top = () => modals.at(-1) === root;
    const keyboard = (event: KeyboardEvent) => {
      if (!top() || event.defaultPrevented || event.isComposing) return;
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        close.current();
        return;
      }
      if (event.key !== "Tab") return;
      const nodes = focusableWithin(root);
      const index = nodes.indexOf(document.activeElement as HTMLElement);
      const target = trapTarget(nodes.length, index, event.shiftKey);
      if (target !== undefined) {
        event.preventDefault();
        nodes[target].focus();
      } else if (!nodes.length) event.preventDefault();
    };
    const focusIn = (event: FocusEvent) => {
      const target = event.target as Node | null;
      if (
        !top() ||
        !target ||
        root.contains(target) ||
        (target instanceof Element && target.closest(`[${MODAL_KEEP}]`))
      )
        return;
      first();
    };
    document.addEventListener("keydown", keyboard);
    document.addEventListener("focusin", focusIn);
    return () => {
      document.removeEventListener("keydown", keyboard);
      document.removeEventListener("focusin", focusIn);
      const index = modals.lastIndexOf(root);
      if (index >= 0) modals.splice(index, 1);
      restoreInert?.();
      const current = document.activeElement;
      if (
        previous?.isConnected &&
        (!current || current === document.body || root.contains(current))
      ) {
        previous.focus();
        // A closed menu hides its items; fall back to the menu's own toggle.
        if (document.activeElement !== previous)
          previous.closest("details")?.querySelector("summary")?.focus();
      }
    };
  }, [active, inertBackground]);
}

export function prefersReducedMotion() {
  return (
    typeof matchMedia === "function" &&
    matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}
/** Smooth scrolling unless the person asked for reduced motion. */
export function scrollBehavior(): ScrollBehavior {
  return prefersReducedMotion() ? "auto" : "smooth";
}

const terminalReaderKey = "twr.terminal.screenReader";
export const terminalReaderEvent = "twr:terminal-screen-reader";
export function terminalScreenReader() {
  try {
    return localStorage.getItem(terminalReaderKey) === "true";
  } catch {
    return false;
  }
}
export function setTerminalScreenReader(enabled: boolean) {
  try {
    localStorage.setItem(terminalReaderKey, String(enabled));
  } catch {}
  window.dispatchEvent(new CustomEvent(terminalReaderEvent));
}

/** Announcement for a request that needs the person. */
export function attentionAnnouncement(item: {
  reason: string;
  summary?: string;
  threadName?: string;
  /** A loop's setup card (its reason is `review`) or a stuck loop's item (`error`). */
  loop_setup?: boolean;
  loop_stuck?: boolean;
}) {
  const who = item.threadName || "twr";
  const kind = item.loop_setup
    ? "asks you to choose a loop's settings"
    : item.loop_stuck
      ? "has a loop that is stuck"
      : item.reason === "permission"
        ? "asks for permission"
        : item.reason === "question"
          ? "has a question"
          : item.reason === "review"
            ? "finished work to review"
            : "needs you";
  const summary = (item.summary || "").replace(/\s+/g, " ").trim();
  return `Needs you: ${who} ${kind}${summary ? ": " + summary.slice(0, 160) : ""}`;
}

/** Announcement for the end of an agent turn. */
export function turnAnnouncement(
  name: string,
  result: string | undefined,
): string {
  if (result === "error") return `${name} stopped with an error`;
  if (result === "interrupted") return `${name} was interrupted`;
  return `${name} finished responding`;
}
