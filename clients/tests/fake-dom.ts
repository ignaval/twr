/* A small stand-in for the browser DOM, enough for react-dom to mount the shared UI in
 * vitest's Node environment and for a test to type into it: elements, text, attributes,
 * capture and bubble event dispatch to `window`, focus, and simple selectors (tags,
 * classes, ids, attributes, descendants). Layout is absent: every size reads as zero.
 * The clients take no DOM library as a dependency; this keeps the component tests free of
 * one. Install it before importing react-dom (react-dom checks for a DOM when it loads). */

type Listener = {
  fn: EventListenerOrEventListenerObject;
  capture: boolean;
  once: boolean;
};

export class FakeEvent {
  type: string;
  bubbles: boolean;
  cancelable: boolean;
  composed = false;
  defaultPrevented = false;
  isTrusted = true;
  timeStamp = performance.now();
  eventPhase = 0;
  target: any = null;
  currentTarget: any = null;
  stopped = false;
  stoppedNow = false;
  [key: string]: any;
  constructor(type: string, init: Record<string, any> = {}) {
    this.type = type;
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    for (const [key, value] of Object.entries(init))
      if (!["bubbles", "cancelable"].includes(key)) this[key] = value;
  }
  preventDefault() {
    if (this.cancelable) this.defaultPrevented = true;
  }
  stopPropagation() {
    this.stopped = true;
  }
  stopImmediatePropagation() {
    this.stopped = true;
    this.stoppedNow = true;
  }
  composedPath() {
    const path: any[] = [];
    for (let node = this.target; node; node = parentOf(node)) path.push(node);
    return path;
  }
}

function parentOf(node: any): any {
  if (node instanceof FakeDocument) return node.defaultView;
  return node?.parentNode ?? null;
}

class FakeEventTarget {
  private listeners = new Map<string, Listener[]>();
  addEventListener(
    type: string,
    fn: EventListenerOrEventListenerObject | null,
    options?: boolean | AddEventListenerOptions,
  ) {
    if (!fn) return;
    const capture = typeof options === "boolean" ? options : !!options?.capture;
    const once = typeof options === "object" && !!options?.once;
    const list = this.listeners.get(type) || [];
    if (!list.some((l) => l.fn === fn && l.capture === capture))
      list.push({ fn, capture, once });
    this.listeners.set(type, list);
  }
  removeEventListener(
    type: string,
    fn: EventListenerOrEventListenerObject | null,
    options?: boolean | EventListenerOptions,
  ) {
    const capture = typeof options === "boolean" ? options : !!options?.capture;
    const list = this.listeners.get(type);
    if (list)
      this.listeners.set(
        type,
        list.filter((l) => !(l.fn === fn && l.capture === capture)),
      );
  }
  /** Runs this target's listeners for one phase of an event. */
  invoke(event: FakeEvent, capture: boolean) {
    for (const listener of [...(this.listeners.get(event.type) || [])]) {
      if (listener.capture !== capture) continue;
      if (listener.once)
        this.removeEventListener(event.type, listener.fn, capture);
      event.currentTarget = this;
      if (typeof listener.fn === "function")
        listener.fn.call(this, event as any);
      else listener.fn.handleEvent(event as any);
      if (event.stoppedNow) return;
    }
  }
  dispatchEvent(event: FakeEvent) {
    event.target = this;
    const path = event.composedPath();
    event.eventPhase = 1;
    for (const node of [...path].reverse().slice(0, -1)) {
      node.invoke(event, true);
      if (event.stopped) return !event.defaultPrevented;
    }
    event.eventPhase = 2;
    this.invoke(event, true);
    if (!event.stopped) this.invoke(event, false);
    event.eventPhase = 3;
    if (event.bubbles)
      for (const node of path.slice(1)) {
        if (event.stopped) break;
        node.invoke(event, false);
      }
    event.eventPhase = 0;
    event.currentTarget = null;
    return !event.defaultPrevented;
  }
}

export class FakeNode extends FakeEventTarget {
  static ELEMENT_NODE = 1;
  static TEXT_NODE = 3;
  static COMMENT_NODE = 8;
  static DOCUMENT_NODE = 9;
  static DOCUMENT_FRAGMENT_NODE = 11;
  ELEMENT_NODE = 1;
  TEXT_NODE = 3;
  DOCUMENT_NODE = 9;
  nodeType = 0;
  nodeName = "";
  childNodes: FakeNode[] = [];
  parentNode: FakeNode | null = null;
  ownerDocument: FakeDocument | null;
  constructor(ownerDocument: FakeDocument | null) {
    super();
    this.ownerDocument = ownerDocument;
  }
  get parentElement(): FakeElement | null {
    return this.parentNode instanceof FakeElement ? this.parentNode : null;
  }
  get firstChild(): FakeNode | null {
    return this.childNodes[0] ?? null;
  }
  get lastChild(): FakeNode | null {
    return this.childNodes.at(-1) ?? null;
  }
  get nextSibling(): FakeNode | null {
    const siblings = this.parentNode?.childNodes;
    return siblings?.[siblings.indexOf(this) + 1] ?? null;
  }
  get previousSibling(): FakeNode | null {
    const siblings = this.parentNode?.childNodes;
    const index = siblings?.indexOf(this) ?? -1;
    return index > 0 ? siblings![index - 1] : null;
  }
  get isConnected(): boolean {
    let node: FakeNode | null = this;
    while (node && !(node instanceof FakeDocument)) node = node.parentNode;
    return !!node;
  }
  hasChildNodes() {
    return this.childNodes.length > 0;
  }
  appendChild<T extends FakeNode>(child: T): T {
    return this.insertBefore(child, null);
  }
  insertBefore<T extends FakeNode>(child: T, before: FakeNode | null): T {
    child.parentNode?.removeChild(child);
    const index = before ? this.childNodes.indexOf(before) : -1;
    if (index < 0) this.childNodes.push(child);
    else this.childNodes.splice(index, 0, child);
    child.parentNode = this;
    return child;
  }
  removeChild<T extends FakeNode>(child: T): T {
    const index = this.childNodes.indexOf(child);
    if (index >= 0) this.childNodes.splice(index, 1);
    child.parentNode = null;
    if (
      this.ownerDocument?.activeElement &&
      child.contains(this.ownerDocument.activeElement)
    )
      this.ownerDocument.activeElement = this.ownerDocument.body;
    return child;
  }
  remove() {
    this.parentNode?.removeChild(this);
  }
  contains(other: FakeNode | null): boolean {
    for (let node = other; node; node = node.parentNode)
      if (node === this) return true;
    return false;
  }
  getRootNode(): FakeNode {
    let node: FakeNode = this;
    while (node.parentNode) node = node.parentNode;
    return node;
  }
  get textContent(): string {
    return this.childNodes.map((child) => child.textContent).join("");
  }
  set textContent(text: string) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes = [];
    if (text) this.appendChild(this.ownerDocument!.createTextNode(text));
  }
}

export class FakeText extends FakeNode {
  nodeType = 3;
  nodeName = "#text";
  nodeValue: string;
  constructor(ownerDocument: FakeDocument, text: string) {
    super(ownerDocument);
    this.nodeValue = text;
  }
  get data() {
    return this.nodeValue;
  }
  set data(text: string) {
    this.nodeValue = text;
  }
  get textContent() {
    return this.nodeValue;
  }
  set textContent(text: string) {
    this.nodeValue = text;
  }
}

export class FakeComment extends FakeText {
  nodeType = 8;
  nodeName = "#comment";
}

function styleDeclaration() {
  const style: Record<string, any> = {};
  Object.defineProperties(style, {
    setProperty: {
      value: (name: string, value: string) => (style[name] = value),
    },
    removeProperty: { value: (name: string) => delete style[name] },
    getPropertyValue: { value: (name: string) => style[name] ?? "" },
  });
  return style;
}

/** One compound selector (`button.active[aria-selected=true]`): what it requires. */
type Compound = {
  tag?: string;
  id?: string;
  classes: string[];
  attributes: { name: string; value?: string }[];
  unsupported?: boolean;
};

function compound(text: string): Compound {
  const result: Compound = { classes: [], attributes: [] };
  const pattern =
    /^([a-zA-Z][\w-]*|\*)|#([\w-]+)|\.([\w-]+)|\[([\w-]+)(?:=(?:"([^"]*)"|'([^']*)'|([^\]]*)))?\]|(:.*)/g;
  let rest = text;
  for (const match of text.matchAll(pattern)) {
    rest = rest.replace(match[0], "");
    if (match[1] && match[1] !== "*") result.tag = match[1].toLowerCase();
    if (match[2]) result.id = match[2];
    if (match[3]) result.classes.push(match[3]);
    if (match[4])
      result.attributes.push({
        name: match[4],
        value: match[5] ?? match[6] ?? match[7],
      });
    if (match[8]) result.unsupported = true;
  }
  if (rest.trim()) result.unsupported = true;
  return result;
}

function matchesCompound(element: FakeElement, selector: Compound) {
  if (selector.unsupported) return false;
  if (selector.tag && element.localName !== selector.tag) return false;
  if (selector.id && element.getAttribute("id") !== selector.id) return false;
  const classes = (element.getAttribute("class") || "").split(/\s+/);
  if (selector.classes.some((name) => !classes.includes(name))) return false;
  return selector.attributes.every(({ name, value }) =>
    value === undefined
      ? element.hasAttribute(name)
      : element.getAttribute(name) === value,
  );
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  return selector.split(",").some((alternative) => {
    // Descendant combinators: the spaces outside an attribute's brackets.
    const parts = (
      alternative.trim().match(/(?:\[[^\]]*\]|[^\s[])+/g) || []
    ).map(compound);
    if (!matchesCompound(element, parts.at(-1)!)) return false;
    let index = parts.length - 2;
    for (
      let node = element.parentElement;
      node && index >= 0;
      node = node.parentElement
    )
      if (matchesCompound(node, parts[index])) index--;
    return index < 0;
  });
}

export class FakeElement extends FakeNode {
  nodeType = 1;
  localName: string;
  tagName: string;
  namespaceURI: string;
  style = styleDeclaration();
  dataset: Record<string, string> = {};
  scrollTop = 0;
  scrollLeft = 0;
  scrollHeight = 0;
  scrollWidth = 0;
  clientHeight = 0;
  clientWidth = 0;
  offsetHeight = 0;
  offsetWidth = 0;
  offsetTop = 0;
  offsetLeft = 0;
  private attributeValues = new Map<string, string>();
  constructor(ownerDocument: FakeDocument, name: string, namespace: string) {
    super(ownerDocument);
    this.localName = name.toLowerCase();
    this.tagName = this.nodeName =
      namespace === XHTML ? name.toUpperCase() : name;
    this.namespaceURI = namespace;
  }
  setAttribute(name: string, value: unknown) {
    this.attributeValues.set(name.toLowerCase(), String(value));
  }
  setAttributeNS(_namespace: string | null, name: string, value: unknown) {
    this.setAttribute(name, value);
  }
  getAttribute(name: string) {
    return this.attributeValues.get(name.toLowerCase()) ?? null;
  }
  hasAttribute(name: string) {
    return this.attributeValues.has(name.toLowerCase());
  }
  removeAttribute(name: string) {
    this.attributeValues.delete(name.toLowerCase());
  }
  removeAttributeNS(_namespace: string | null, name: string) {
    this.removeAttribute(name);
  }
  toggleAttribute(name: string, force?: boolean) {
    const on = force ?? !this.hasAttribute(name);
    if (on) this.setAttribute(name, "");
    else this.removeAttribute(name);
    return on;
  }
  get attributes() {
    return [...this.attributeValues].map(([name, value]) => ({ name, value }));
  }
  get id() {
    return this.getAttribute("id") || "";
  }
  set id(value: string) {
    this.setAttribute("id", value);
  }
  get className() {
    return this.getAttribute("class") || "";
  }
  set className(value: string) {
    this.setAttribute("class", value);
  }
  get classList() {
    const names = () => this.className.split(/\s+/).filter(Boolean);
    return {
      contains: (name: string) => names().includes(name),
      add: (...add: string[]) =>
        (this.className = [...new Set([...names(), ...add])].join(" ")),
      remove: (...remove: string[]) =>
        (this.className = names()
          .filter((name) => !remove.includes(name))
          .join(" ")),
      toggle: (name: string, force?: boolean) => {
        const on = force ?? !names().includes(name);
        if (on) this.classList.add(name);
        else this.classList.remove(name);
        return on;
      },
    };
  }
  get disabled() {
    return this.hasAttribute("disabled");
  }
  set disabled(value: boolean) {
    this.toggleAttribute("disabled", !!value);
  }
  get tabIndex() {
    const value = this.getAttribute("tabindex");
    return value == null ? 0 : Number(value);
  }
  set tabIndex(value: number) {
    this.setAttribute("tabindex", String(value));
  }
  get children() {
    return this.childNodes.filter(
      (node): node is FakeElement => node instanceof FakeElement,
    );
  }
  get innerHTML() {
    return this.textContent;
  }
  set innerHTML(text: string) {
    this.textContent = text;
  }
  matches(selector: string) {
    return matchesSelector(this, selector);
  }
  closest(selector: string) {
    for (let node: FakeElement | null = this; node; node = node.parentElement)
      if (node.matches(selector)) return node;
    return null;
  }
  querySelectorAll(selector: string): FakeElement[] {
    const found: FakeElement[] = [];
    const walk = (node: FakeNode) => {
      for (const child of node.childNodes)
        if (child instanceof FakeElement) {
          if (child.matches(selector)) found.push(child);
          walk(child);
        }
    };
    walk(this);
    return found;
  }
  querySelector(selector: string) {
    return this.querySelectorAll(selector)[0] ?? null;
  }
  getElementsByTagName(name: string) {
    return this.querySelectorAll(name);
  }
  focus() {
    if (this.ownerDocument && !this.disabled)
      this.ownerDocument.activeElement = this;
  }
  blur() {
    if (this.ownerDocument?.activeElement === this)
      this.ownerDocument.activeElement = this.ownerDocument.body;
  }
  click() {
    this.dispatchEvent(
      new FakeEvent("click", { bubbles: true, cancelable: true, button: 0 }),
    );
  }
  getBoundingClientRect() {
    return {
      x: 0,
      y: 0,
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      width: 0,
      height: 0,
      toJSON() {},
    };
  }
  getClientRects() {
    return [];
  }
  scrollIntoView() {}
  scrollTo() {}
  scrollBy() {}
  animate() {
    return { cancel() {}, finished: Promise.resolve(), onfinish: null };
  }
}

/** Form controls keep their value as a plain property, as React expects of a DOM node. */
export class FakeFormControl extends FakeElement {
  value = "";
  defaultValue = "";
  checked = false;
  defaultChecked = false;
  selectionStart = 0;
  selectionEnd = 0;
  files: unknown[] = [];
  setSelectionRange(start: number, end: number) {
    this.selectionStart = start;
    this.selectionEnd = end;
  }
  select() {}
  get form() {
    return this.closest("form");
  }
}
export class FakeInput extends FakeFormControl {}
export class FakeTextArea extends FakeFormControl {}
export class FakeDetails extends FakeElement {
  get open() {
    return this.hasAttribute("open");
  }
  set open(value: boolean) {
    this.toggleAttribute("open", !!value);
  }
}
export class FakeDialog extends FakeDetails {
  showModal() {
    this.open = true;
  }
  show() {
    this.open = true;
  }
  close() {
    this.open = false;
  }
}
export class FakeIFrame extends FakeElement {}

const XHTML = "http://www.w3.org/1999/xhtml";

export class FakeDocument extends FakeNode {
  nodeType = 9;
  nodeName = "#document";
  documentElement: FakeElement;
  head: FakeElement;
  body: FakeElement;
  activeElement: FakeElement | null = null;
  defaultView: any = null;
  visibilityState = "visible";
  hidden = false;
  readyState = "complete";
  // react-dom feature-detects the input event as `"oninput" in document`.
  oninput = null;
  constructor() {
    super(null);
    this.ownerDocument = null;
    this.documentElement = this.createElement("html");
    this.head = this.createElement("head");
    this.body = this.createElement("body");
    this.appendChild(this.documentElement);
    this.documentElement.appendChild(this.head);
    this.documentElement.appendChild(this.body);
    this.activeElement = this.body;
  }
  createElement(name: string) {
    return this.createElementNS(XHTML, name);
  }
  createElementNS(namespace: string, name: string) {
    const kinds: Record<string, typeof FakeElement> = {
      input: FakeInput,
      textarea: FakeTextArea,
      select: FakeFormControl,
      details: FakeDetails,
      dialog: FakeDialog,
      iframe: FakeIFrame,
    };
    const Kind =
      (namespace === XHTML && kinds[name.toLowerCase()]) || FakeElement;
    return new Kind(this, name, namespace);
  }
  createTextNode(text: string) {
    return new FakeText(this, text);
  }
  createComment(text: string) {
    return new FakeComment(this, text);
  }
  createDocumentFragment() {
    return new FakeElement(this, "#fragment", XHTML);
  }
  getElementById(id: string) {
    return this.documentElement.querySelector(`[id="${id}"]`);
  }
  querySelector(selector: string) {
    return this.documentElement.matches(selector)
      ? this.documentElement
      : this.documentElement.querySelector(selector);
  }
  querySelectorAll(selector: string) {
    return this.documentElement.querySelectorAll(selector);
  }
  getElementsByTagName(name: string) {
    return this.querySelectorAll(name);
  }
  hasFocus() {
    return true;
  }
  getSelection() {
    return null;
  }
}

class Inert {
  observe() {}
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
}

/** Puts a fresh document and window on the global object; returns the document. */
export function installFakeDOM(width = 1200) {
  const document = new FakeDocument();
  const window = new FakeEventTarget() as any;
  document.defaultView = window;
  const frames = new Map<number, ReturnType<typeof setTimeout>>();
  let frame = 0;
  Object.assign(window, {
    document,
    window,
    self: window,
    innerWidth: width,
    innerHeight: 900,
    devicePixelRatio: 1,
    location: { href: "file:///index.html", protocol: "file:", hash: "" },
    navigator: globalThis.navigator,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    setInterval: globalThis.setInterval,
    clearInterval: globalThis.clearInterval,
    queueMicrotask: globalThis.queueMicrotask,
    performance: globalThis.performance,
    matchMedia: (query: string) => ({
      matches: false,
      media: query,
      addEventListener() {},
      removeEventListener() {},
      addListener() {},
      removeListener() {},
    }),
    getComputedStyle: () => ({ getPropertyValue: () => "" }),
    requestAnimationFrame: (callback: (time: number) => void) => {
      const id = ++frame;
      frames.set(
        id,
        setTimeout(() => {
          frames.delete(id);
          callback(performance.now());
        }, 0),
      );
      return id;
    },
    cancelAnimationFrame: (id: number) => {
      clearTimeout(frames.get(id));
      frames.delete(id);
    },
    scrollTo() {},
    getSelection: () => null,
    ResizeObserver: Inert,
    MutationObserver: Inert,
    IntersectionObserver: Inert,
    Node: FakeNode,
    Element: FakeElement,
    HTMLElement: FakeElement,
    SVGElement: FakeElement,
    HTMLInputElement: FakeInput,
    HTMLTextAreaElement: FakeTextArea,
    HTMLDetailsElement: FakeDetails,
    HTMLDialogElement: FakeDialog,
    HTMLIFrameElement: FakeIFrame,
    Event: FakeEvent,
    CustomEvent: FakeEvent,
    KeyboardEvent: FakeEvent,
    MouseEvent: FakeEvent,
    FocusEvent: FakeEvent,
    InputEvent: FakeEvent,
  });
  const storage = new Map<string, string>();
  window.localStorage = window.sessionStorage = {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, String(value)),
    removeItem: (key: string) => storage.delete(key),
    clear: () => storage.clear(),
    key: (index: number) => [...storage.keys()][index] ?? null,
    get length() {
      return storage.size;
    },
  };
  for (const name of Object.keys(window))
    if (
      ![
        "navigator",
        "window",
        "self",
        "setTimeout",
        "clearTimeout",
        "setInterval",
        "clearInterval",
        "queueMicrotask",
        "performance",
      ].includes(name)
    )
      Object.defineProperty(globalThis, name, {
        value: window[name],
        configurable: true,
        writable: true,
      });
  Object.defineProperty(globalThis, "window", {
    value: window,
    configurable: true,
    writable: true,
  });
  return document;
}

/** Types `text` at the end of a text field, one character per input event. */
export function typeInto(field: FakeFormControl, text: string) {
  for (const character of text) {
    field.value += character;
    field.selectionStart = field.selectionEnd = field.value.length;
    field.dispatchEvent(new FakeEvent("input", { bubbles: true }));
  }
}

/** Replaces a text field's whole value, as pasting over a selection does. */
export function replaceValue(field: FakeFormControl, text: string) {
  field.value = text;
  field.selectionStart = field.selectionEnd = text.length;
  field.dispatchEvent(new FakeEvent("input", { bubbles: true }));
}

/** Presses a key in an element: a cancelable keydown that bubbles to the window. */
export function press(
  element: FakeElement,
  key: string,
  init: Record<string, unknown> = {},
) {
  return element.dispatchEvent(
    new FakeEvent("keydown", {
      bubbles: true,
      cancelable: true,
      key,
      keyCode: key === "Enter" ? 13 : 0,
      ...init,
    }),
  );
}
