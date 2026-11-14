import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopEvent, Json, Thread } from "../shared/types";
import { SUGGEST_SETTLE_MS } from "../shared/assistance";
import {
  FakeEvent,
  installFakeDOM,
  press,
  replaceValue,
  typeInto,
  type FakeDocument,
  type FakeElement,
  type FakeTextArea,
} from "./fake-dom";

/* The whole App on a fake host and a stand-in DOM (fake-dom.ts), driven through its
 * message box as a person would: typing, Enter, the sidebar, Take back. */

// `workspaceEntries` runs once in every render of App's body, and the conversation calls
// `useVirtualizer` in every render of its own: counting the calls counts the renders.
const renders = vi.hoisted(() => ({ app: 0, conversation: 0 }));
vi.mock("../shared/workspaceTree", async (importOriginal) => {
  const real = await importOriginal<typeof import("../shared/workspaceTree")>();
  return {
    ...real,
    workspaceEntries: (...args: Parameters<typeof real.workspaceEntries>) => {
      renders.app++;
      return real.workspaceEntries(...args);
    },
  };
});
vi.mock("@tanstack/react-virtual", async (importOriginal) => {
  const real = await importOriginal<typeof import("@tanstack/react-virtual")>();
  return {
    ...real,
    useVirtualizer: ((...args: Parameters<typeof real.useVirtualizer>) => {
      renders.conversation++;
      return real.useVirtualizer(...args);
    }) as typeof real.useVirtualizer,
  };
});

const thread = (fields: Partial<Thread>): Thread => ({
  runtime: "fake",
  box_name: "local",
  mode: "default",
  status: "idle",
  accent: "#3366ff",
  is_orchestrator: false,
  ...fields,
  id: fields.id!,
  name: fields.name!,
});
const views: Record<string, Thread[]> = {
  default: [
    thread({ id: "orch", name: "orchestrator", is_orchestrator: true }),
    thread({ id: "w1", name: "builder" }),
    thread({ id: "w2", name: "tester" }),
  ],
  other: [thread({ id: "orch2", name: "orchestrator", is_orchestrator: true })],
};

/** What a test changes in the fake host: everything else answers as an idle hub. */
type HostOptions = {
  /** Answers a method before the defaults do; `undefined` falls through to them. */
  rpc?: (method: string, params: Json) => Promise<Json> | undefined;
  /** The configuration's settings (`configRead`). */
  settings?: Json;
  /** Every conversation's transcript. */
  events?: Json[];
  /** The device's encrypted storage at start. */
  secure?: Record<string, string>;
  /** The connection takes uploads. */
  uploads?: boolean;
};

function fakeHost(options: HostOptions = {}) {
  const listeners = new Set<(event: DesktopEvent) => void>();
  const secure = new Map<string, string>(Object.entries(options.secure || {}));
  const sent: { method: string; params: Json }[] = [];
  const host = {
    sent,
    secure,
    /** What `thread.queue` lists, by thread. */
    queues: {} as Record<string, Json[]>,
    emit: (event: DesktopEvent) =>
      listeners.forEach((listener) => listener(event)),
    secureRead: async (key: string) => secure.get(key) ?? null,
    secureWrite: async (key: string, value: string) =>
      void secure.set(key, value),
    secureDelete: async (key: string) => void secure.delete(key),
    bootstrap: async () => ({
      workspace: "default",
      hostId: "composer-test",
      platform: "linux",
      home: "/home/test",
      version: "0.0.0",
      theme: null,
      terminals: [],
    }),
    connect: async () => ({}),
    rpc: async (method: string, params: Json = {}) => {
      const answer = options.rpc?.(method, params);
      if (answer !== undefined) return answer;
      if (method === "thread.transcript")
        return { events: options.events || [], more: false };
      if (method === "desktop.slash_commands")
        return [
          { id: "jobs", title: "Jobs" },
          { id: "usage", title: "Usage" },
        ];
      if (method === "thread.queue") return host.queues[params.thread] || [];
      if (method === "message.withdraw") {
        const queue = host.queues[params.thread] || [];
        const message = queue.find((m) => m.ref === params.ref);
        host.queues[params.thread] = queue.filter((m) => m !== message);
        return { withdrawn: true, prompt: message?.prompt };
      }
      if (method === "say" || method === "thread.send") {
        sent.push({ method, params });
        return { action: "sent" };
      }
      return {};
    },
    cli: async (args: string[]) =>
      args.join(" ") === "ws list --json"
        ? { stdout: JSON.stringify(Object.keys(views)), stderr: "", code: 0 }
        : { stdout: "", stderr: "", code: 0 },
    catalog: async () => [],
    configRead: async () => ({
      content: "",
      hash: "",
      path: "",
      settings: options.settings || {},
    }),
    configSave: async () => {},
    configSet: async () => {},
    onEvent: (callback: (event: DesktopEvent) => void) => {
      listeners.add(callback);
      return () => listeners.delete(callback);
    },
    quit: async () => {},
    copyText: async () => {},
    openExternal: async () => {},
    reveal: async () => {},
    exportText: async () => {},
    pickDirectory: async () => null,
    terminalOpen: async () => ({}),
    terminalFocus() {},
    terminalAck() {},
    terminalWrite() {},
    terminalResize() {},
    terminalClose: async () => {},
    terminalRead: async () => ({ data: "", sequence: 0 }),
    uploadFile: options.uploads
      ? async (file: { name: string }, thread: string) => ({
          path: "/uploads/" + file.name,
          thread,
        })
      : undefined,
    /** Delivers a message from the device's outbox; unset, the app never uses one. */
    sendMessage: undefined as ((message: Json) => Promise<Json>) | undefined,
  };
  return host;
}

type Mounted = Awaited<ReturnType<typeof mount>>;
async function mount(
  options: HostOptions & {
    /** This device's localStorage at start. */
    local?: Record<string, string>;
    /** The paired hub's id, as the mobile app marks it on the page. */
    hub?: string;
  } = {},
) {
  const document = installFakeDOM() as FakeDocument;
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  for (const [key, value] of Object.entries(options.local || {}))
    localStorage.setItem(key, value);
  if (options.hub) document.documentElement.dataset.hub = options.hub;
  const host = fakeHost(options);
  (window as any).twr = host;
  // react-dom looks for a DOM as it loads: import it once the stand-in is in place.
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const App = (await import("../shared/App")).default;
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const settle = () =>
    act(async () => {
      for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
    });
  /** Lets `ms` of real time pass, then what it set off. */
  const wait = async (ms: number) => {
    await act(() => new Promise<void>((r) => setTimeout(r, ms)));
    await settle();
  };
  /** Delivers a workspace's live view, as the hub does once it is connected. */
  const live = async (workspace: string, threads = views[workspace]) => {
    await act(async () => {
      host.emit({ type: "connection", connected: true } as DesktopEvent);
      host.emit({
        type: "state",
        state: {
          threads,
          attention: [],
          decisions: [],
          decisions_seen: 0,
          head: 1,
        },
      } as DesktopEvent);
    });
    await settle();
  };
  await act(async () => root.render(createElement(App)));
  await settle();
  await live("default");
  const all = (selector: string) => document.querySelectorAll(selector);
  const find = (selector: string, text?: string) => {
    const node = all(selector).find(
      (node) => text === undefined || node.textContent.includes(text),
    );
    if (!node) throw new Error(`Nothing matches ${selector} ${text ?? ""}`);
    return node as FakeElement;
  };
  const click = async (node: FakeElement, init: Record<string, any> = {}) => {
    await act(async () => {
      node.dispatchEvent(
        new FakeEvent("click", {
          bubbles: true,
          cancelable: true,
          button: 0,
          detail: 1,
          ...init,
        }),
      );
    });
    await settle();
  };
  const box = () => find(".composer textarea") as FakeTextArea;
  return {
    document,
    host,
    act,
    settle,
    wait,
    live,
    all,
    find,
    click,
    box,
    type: (text: string) => act(async () => typeInto(box(), text)),
    /** Replaces the whole text, as a paste over a selection does. */
    replace: (text: string) => act(async () => replaceValue(box(), text)),
    /** Loses or regains the hub. */
    connection: async (connected: boolean) => {
      await act(async () =>
        host.emit({ type: "connection", connected } as DesktopEvent),
      );
      await settle();
    },
    press: async (key: string, init?: Record<string, unknown>) => {
      await act(async () => void press(box(), key, init));
      await settle();
    },
    unmount: () => act(async () => root.unmount()),
    /** Opens a worker's conversation, as a double-click on its sidebar card does. */
    talkTo: async (name: string) => {
      const card = find(".sidebar .thread-card", name);
      await act(async () => {
        card.dispatchEvent(
          new FakeEvent("dblclick", { bubbles: true, cancelable: true }),
        );
      });
      await settle();
    },
    /** Switches to another workspace's orchestrator from the sidebar. */
    switchTo: async (workspace: string) => {
      await click(
        find(`[data-workspace=${workspace}] .workspace-orchestrator`),
      );
      await live(workspace);
    },
  };
}

let mounted: Mounted;
const app = () => mounted;

describe("the message box", () => {
  beforeEach(async () => {
    vi.resetModules();
    renders.app = 0;
    renders.conversation = 0;
    mounted = await mount();
  });
  afterEach(async () => {
    await mounted.unmount();
  });

  it("types without re-rendering the rest of the app", async () => {
    const box = app().box();
    expect(box.disabled).toBe(false);
    expect(renders.conversation).toBeGreaterThan(0);
    // The first character turns the empty box into a draft: App renders for that, to stop
    // asking for a reply suggestion (and once more as that clears the suggestion, which
    // React renders before it sees the state is unchanged).
    const before = renders.app;
    await app().type("h");
    expect(renders.app - before).toBeLessThanOrEqual(2);
    const app0 = renders.app;
    const conversation0 = renders.conversation;
    const text = "ello, orchestrator: please build the thing";
    await app().type(text);
    expect(box.value).toBe("h" + text);
    expect(renders.app - app0).toBe(0);
    expect(renders.conversation - conversation0).toBe(0);
    // `/` and `@` suggestions are the composer's own: still no App render.
    await app().type(" @bu");
    expect(app().find("#twr-mentions").textContent).toContain("@builder");
    expect(renders.app - app0).toBe(0);
  });

  it("keeps a draft for each thread when you switch between them", async () => {
    await app().type("for the orchestrator");
    await app().talkTo("builder");
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
    expect(app().box().value).toBe("");
    await app().type("for the builder");
    await app().talkTo("tester");
    expect(app().box().value).toBe("");
    await app().click(app().find('[aria-label="Return to orchestrator"]'));
    expect(app().box().getAttribute("aria-label")).toBe("Message orchestrator");
    expect(app().box().value).toBe("for the orchestrator");
    await app().talkTo("builder");
    expect(app().box().value).toBe("for the builder");
  });

  it("keeps each workspace's draft when you switch workspaces", async () => {
    await app().type("PRESERVE_DURING_SWITCH");
    await app().switchTo("other");
    expect(app().box().value).toBe("");
    await app().type("in the other workspace");
    await app().switchTo("default");
    expect(app().box().value).toBe("PRESERVE_DURING_SWITCH");
    await app().switchTo("other");
    expect(app().box().value).toBe("in the other workspace");
  });

  it("clears only the sent destination's draft", async () => {
    await app().talkTo("builder");
    await app().type("kept for the builder");
    await app().click(app().find('[aria-label="Return to orchestrator"]'));
    await app().type("Build the login page");
    await app().press("Enter");
    expect(app().host.sent).toEqual([
      {
        method: "say",
        params: expect.objectContaining({
          message: "Build the login page",
          to: "orchestrator",
          suggested: false,
        }),
      },
    ]);
    expect(app().box().value).toBe("");
    await app().talkTo("builder");
    expect(app().box().value).toBe("kept for the builder");
    await app().press("Enter");
    expect(app().host.sent[1]).toEqual({
      method: "thread.send",
      params: { thread: "w1", prompt: "kept for the builder" },
    });
    expect(app().box().value).toBe("");
  });

  it("puts a message taken back from the queue into the box", async () => {
    app().host.queues.w1 = [{ ref: 7, prompt: "queued words", mine: true }];
    await app().talkTo("builder");
    await app().type("already here");
    await app().click(app().find(".queued-message button", "Take back"));
    expect(app().box().value).toBe("already here\n\nqueued words");
  });

  it("saves the drafts on this device after typing stops", async () => {
    await app().type("save me");
    await app().act(() => new Promise((r) => setTimeout(r, 450)));
    await app().settle();
    const saved = [...app().host.secure.values()].join("\n");
    expect(saved).toContain("save me");
    expect(saved).toContain("default:orchestrator");
  });
});

/* Every other reader and writer of a draft, as it behaved while App held the drafts in its
 * own state: each of these passes on the code before the composer moved out too. */
describe("the message box, at its edges", () => {
  afterEach(async () => {
    await mounted.unmount();
  });
  const start = async (options?: Parameters<typeof mount>[0]) => {
    vi.resetModules();
    mounted = await mount(options);
  };
  /** The drafts saved on the device. */
  const saved = (): Record<string, string> | undefined => {
    const raw = app().host.secure.get("offline-pending:composer-test");
    return raw ? JSON.parse(raw).drafts : undefined;
  };
  /** A hub that suggests a reply to the orchestrator's finished turn. */
  const suggesting = {
    settings: {
      tui: { suggest: true, suggest_model: { "claude-code": "haiku" } },
    },
    events: [
      {
        seq: 1,
        id: "1",
        kind: "turn.started",
        turn: 1,
        data: { from: "user" },
      },
      { seq: 2, id: "2", kind: "turn.ended", turn: 1, data: { result: "ok" } },
    ],
    local: {
      "twr.assistance.notice": JSON.stringify({ "claude-code": "haiku" }),
    },
    rpc: (method: string) =>
      method === "suggest.next"
        ? Promise.resolve({ suggestion: "Run the tests" })
        : undefined,
  };
  /** Waits for the suggestion to show, and for as long as Tab needs it shown. */
  const suggested = async () => {
    for (let i = 0; i < 40; i++) {
      if (app().box().getAttribute("placeholder") === "Run the tests") break;
      await app().wait(100);
    }
    expect(app().box().getAttribute("placeholder")).toBe("Run the tests");
    await app().wait(SUGGEST_SETTLE_MS + 50);
  };

  it("takes the suggestion with Tab and says it was sent as suggested", async () => {
    await start(suggesting);
    await suggested();
    await app().press("Tab");
    expect(app().box().value).toBe("Run the tests");
    await app().press("Enter");
    expect(app().host.sent[0].params).toMatchObject({
      message: "Run the tests",
      suggested: true,
    });
    expect(app().box().value).toBe("");
  });

  it("forgets a taken suggestion once it is edited, even back to the same words", async () => {
    await start(suggesting);
    await suggested();
    await app().press("ArrowRight");
    expect(app().box().value).toBe("Run the tests");
    await app().type("!");
    await app().replace("Run the tests");
    await app().press("Enter");
    expect(app().host.sent[0].params.suggested).toBe(false);
  });

  it("shows the suggestion again when the box is emptied", async () => {
    await start(suggesting);
    await suggested();
    await app().type("x");
    expect(app().box().getAttribute("placeholder")).not.toBe("Run the tests");
    await app().replace("");
    await suggested();
  });

  it("runs a command picked with the arrows, and clears the box", async () => {
    await start();
    await app().type("/jo");
    expect(app().find("#twr-slash-suggestions").textContent).toContain("/jobs");
    await app().press("ArrowDown");
    expect(app().box().getAttribute("aria-activedescendant")).toBe(
      "twr-slash-0",
    );
    await app().press("Enter");
    expect(app().all(".composer textarea")).toHaveLength(0);
    await app().switchTo("default");
    expect(app().box().value).toBe("");
  });

  it("keeps the command list closed with Escape while another section shows", async () => {
    await start();
    await app().type("/");
    expect(app().all("#twr-slash-suggestions")).toHaveLength(1);
    await app().press("Escape");
    expect(app().all("#twr-slash-suggestions")).toHaveLength(0);
    await app().click(app().find(".primary-nav button", "Thread grid"));
    expect(app().all(".composer textarea")).toHaveLength(0);
    await app().switchTo("default");
    expect(app().box().value).toBe("/");
    expect(app().all("#twr-slash-suggestions")).toHaveLength(0);
    await app().type("u");
    expect(app().all("#twr-slash-suggestions")).toHaveLength(1);
  });

  it("completes an @ mention chosen with the arrows, and Escape closes the list", async () => {
    await start();
    await app().type("ask @");
    expect(app().find("#twr-mentions").textContent).toContain("@builder");
    await app().press("ArrowDown");
    expect(app().box().getAttribute("aria-activedescendant")).toBe(
      "twr-mention-1",
    );
    await app().press("Enter");
    expect(app().box().value).toBe("ask @tester ");
    expect(app().host.sent).toHaveLength(0);
    await app().type("and @");
    await app().press("Escape");
    expect(app().all("#twr-mentions")).toHaveLength(0);
  });

  it("does not send on the Enter that ends an input method's composition", async () => {
    await start();
    await app().type("にほん");
    await app().press("Enter", { isComposing: true, keyCode: 229 });
    expect(app().host.sent).toHaveLength(0);
    expect(app().box().value).toBe("にほん");
    await app().press("Enter");
    expect(app().host.sent[0].params.message).toBe("にほん");
  });

  it("keeps what is typed while a message is sending", async () => {
    let release!: (value: Json) => void;
    await start({
      rpc: (method, params) => {
        if (method !== "say") return undefined;
        app().host.sent.push({ method, params });
        return new Promise((resolve) => (release = resolve));
      },
    });
    await app().type("first");
    await app().press("Enter");
    expect(app().host.sent).toHaveLength(1);
    await app().type(" and more");
    await app().act(async () => release({ action: "sent" }));
    await app().settle();
    expect(app().box().value).toBe("first and more");
  });

  it("clears the destination a message was sent to, after a switch mid-send", async () => {
    let release!: (value: Json) => void;
    await start({
      rpc: (method, params) => {
        if (method !== "thread.send") return undefined;
        app().host.sent.push({ method, params });
        return new Promise((resolve) => (release = resolve));
      },
    });
    await app().talkTo("builder");
    await app().type("for builder");
    await app().press("Enter");
    await app().talkTo("tester");
    await app().type("for tester");
    await app().act(async () => release({ action: "sent" }));
    await app().settle();
    expect(app().box().value).toBe("for tester");
    await app().talkTo("builder");
    expect(app().box().value).toBe("");
  });

  it("keeps the text of a message the hub holds for a waiting request", async () => {
    await start({
      rpc: (method) =>
        method === "say" ? Promise.resolve({ action: "pending" }) : undefined,
    });
    await app().type("held words");
    await app().press("Enter");
    expect(
      app()
        .find(".primary-nav button", "Needs you")
        .getAttribute("aria-current"),
    ).toBe("page");
    await app().switchTo("default");
    expect(app().box().value).toBe("held words");
  });

  it("keeps the text of a message that fails to send", async () => {
    await start({
      rpc: (method) =>
        method === "say" ? Promise.reject(new Error("refused")) : undefined,
    });
    await app().type("try this");
    await app().press("Enter");
    expect(app().box().value).toBe("try this");
  });

  it("keeps the drafts over a reconnect", async () => {
    await start();
    await app().type("over reconnect");
    await app().connection(false);
    await app().live("default");
    expect(app().box().value).toBe("over reconnect");
  });

  it("goes back to the orchestrator's draft when the open thread is deleted", async () => {
    await start();
    await app().type("orchestrator words");
    await app().talkTo("builder");
    await app().type("builder words");
    await app().live(
      "default",
      views.default.filter((thread) => thread.id !== "w1"),
    );
    expect(app().box().getAttribute("aria-label")).toBe("Message orchestrator");
    expect(app().box().value).toBe("orchestrator words");
  });

  it("adds an uploaded file's path to the draft", async () => {
    await start({ uploads: true });
    await app().type("see");
    const input = app().find(".attachment-button input") as FakeElement & {
      files: unknown[];
    };
    input.files = [
      {
        name: "notes.txt",
        size: 5,
        type: "application/octet-stream",
        slice: () => ({ text: async () => "hello" }),
      },
    ];
    await app().act(async () => {
      input.dispatchEvent(new FakeEvent("change", { bubbles: true }));
    });
    await app().wait(50);
    expect(app().box().value).toBe(
      "see\nUploaded file: notes.txt\n/uploads/notes.txt\n",
    );
  });

  it("shows the drafts saved on the device when it starts again", async () => {
    await start();
    await app().type("survive restart");
    await app().wait(450);
    const secure = Object.fromEntries(app().host.secure);
    await app().unmount();
    await start({ secure });
    expect(app().box().value).toBe("survive restart");
  });

  it("moves the old mobile app's drafts into encrypted storage", async () => {
    await start({
      hub: "h1",
      local: {
        "twr.mobile.drafts.h1": JSON.stringify({
          "default:orchestrator": "old mobile words",
        }),
      },
    });
    expect(app().box().value).toBe("old mobile words");
    expect(localStorage.getItem("twr.mobile.drafts.h1")).toBe(null);
    expect(saved()?.["default:orchestrator"]).toBe("old mobile words");
  });

  it("empties the box when offline storage is reset, and saves nothing older", async () => {
    await start();
    await app().type("to be reset");
    await app().act(async () => {
      window.dispatchEvent(
        new FakeEvent("twr:offline-reset", {
          detail: { hostId: "composer-test" },
        }) as unknown as Event,
      );
    });
    await app().settle();
    expect(app().box().value).toBe("");
    await app().wait(450);
    expect([...app().host.secure.values()].join("\n")).not.toContain(
      "to be reset",
    );
  });

  it("saves a message offline, clears its draft, and moves it back to the draft", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    await app().connection(false);
    await app().type("later please");
    await app().press("Enter");
    expect(app().host.sent).toHaveLength(0);
    expect(app().box().value).toBe("");
    expect(app().find(".outbox-message").textContent).toContain("later please");
    await app().wait(450);
    expect(saved()?.["default:orchestrator"]).toBe("");
    await app().type("new words");
    await app().click(
      app().find(".outbox-message button", "Move to original draft"),
    );
    expect(app().box().value).toBe("new words\n\nlater please");
    expect(app().all(".outbox-message")).toHaveLength(0);
    await app().wait(450);
    expect(saved()?.["default:orchestrator"]).toBe("new words\n\nlater please");
  });

  it("keeps, on screen and on the device, what is typed while the outbox saves", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    await app().connection(false);
    await app().wait(450);
    // Holds the write that puts the message in the outbox.
    const write = app().host.secureWrite;
    let release!: () => void;
    let held = false;
    app().host.secureWrite = async (key: string, value: string) => {
      if (!held && JSON.parse(value).outbox?.length) {
        held = true;
        await new Promise<void>((resolve) => (release = resolve));
      }
      return write(key, value);
    };
    await app().type("queued");
    await app().press("Enter");
    expect(held).toBe(true);
    await app().type(" and more");
    // The drafts wait for the outbox, then save what the box holds.
    await app().wait(450);
    await app().act(async () => release());
    await app().settle();
    expect(app().box().value).toBe("queued and more");
    await app().wait(450);
    expect(saved()?.["default:orchestrator"]).toBe("queued and more");
  });
});

/* Send as implementation loop and /implement (docs/design/loops.md › Desktop and mobile):
 * the message box's other way to send, while its destination is a thread. */
describe("implementation loops from the message box", () => {
  /** What the hub was asked for, by method. */
  const asked: { method: string; params: Json }[] = [];
  let answer: Json = { status: "started", loop: { line: "high · plan" } };
  const loopHost = async () =>
    mount({
      rpc: (method, params) => {
        if (method === "loop.request") {
          asked.push({ method, params });
          return Promise.resolve(answer);
        }
        if (method === "loop.get")
          return Promise.resolve({ started: null, id: "l1" });
        if (method === "desktop.parse_slash") {
          const words = String(params.line).trim().split(/\s+/);
          if (words[0] === "/implement")
            return Promise.resolve({
              kind: "Implement",
              args: { words: words.slice(1), setup: false },
            });
          if (words[0] === "/loop")
            return Promise.reject(
              new Error(
                "/loop never starts a loop: use /implement <task> or /review",
              ),
            );
        }
        return undefined;
      },
    });
  beforeEach(async () => {
    vi.resetModules();
    asked.length = 0;
    answer = { status: "started", loop: { line: "high · plan" } };
    mounted = await loopHost();
  });
  afterEach(async () => {
    await mounted.unmount();
  });
  const sendAsLoop = async () => {
    await app().click(app().find(".send-caret"));
    await app().click(
      app().find(".send-loop-panel button", "Send as implementation loop"),
    );
  };

  it("offers it only when the message goes to a thread", async () => {
    expect(app().all(".send-caret")).toHaveLength(0);
    await app().talkTo("builder");
    const caret = app().find(".send-caret");
    expect(caret.getAttribute("aria-label")).toBe("More ways to send");
    expect(caret.getAttribute("aria-expanded")).toBe("false");
  });

  it("sends the message as the task of a loop on that thread, live", async () => {
    await app().talkTo("builder");
    await app().type("add a CSV export");
    await sendAsLoop();
    expect(asked).toEqual([
      {
        method: "loop.request",
        params: { thread: "w1", kind: "implement", task: "add a CSV export" },
      },
    ]);
    // Never an ordinary message, never the outbox.
    expect(app().host.sent).toEqual([]);
    expect(app().box().value).toBe("");
    expect(app().find(".notice-toast").textContent).toContain(
      "builder started an implementation loop: high · plan.",
    );
  });

  it("keeps the message while the loop's setup card holds it", async () => {
    answer = { status: "needs_setup", attn_id: "a1", revision: 1 };
    await app().talkTo("builder");
    await app().type("add a CSV export");
    await sendAsLoop();
    expect(asked).toHaveLength(1);
    expect(app().find(".notice-toast").textContent).toContain(
      "Choose builder's loop settings on its setup card: it holds your task",
    );
  });

  it("asks before starting on an empty message, whose task is the conversation's", async () => {
    await app().talkTo("builder");
    await sendAsLoop();
    expect(asked).toEqual([]);
    const dialog = app().find("[role=dialog]");
    expect(dialog.textContent).toContain(
      "Start an implementation loop on builder with no task?",
    );
    await app().click(app().find("[role=dialog] button", "Start loop"));
    expect(asked).toEqual([
      { method: "loop.request", params: { thread: "w1", kind: "implement" } },
    ]);
  });

  it("can't be chosen without a live connection, and says why", async () => {
    await app().talkTo("builder");
    await app().type("add a CSV export");
    await app().connection(false);
    await app().click(app().find(".send-caret"));
    const choice = app().find(
      ".send-loop-panel button",
      "Send as implementation loop",
    );
    expect(choice.hasAttribute("disabled")).toBe(true);
    expect(app().find(".send-loop-panel small").textContent).toContain(
      "Loop actions need a live connection",
    );
    expect(asked).toEqual([]);
    expect(app().box().value).toBe("add a CSV export");
  });

  it("runs /implement in a thread's conversation with every word as the task", async () => {
    await app().talkTo("builder");
    await app().type("/implement tester pages faster");
    await app().press("Enter");
    expect(asked).toEqual([
      {
        method: "loop.request",
        params: {
          thread: "w1",
          kind: "implement",
          task: "tester pages faster",
        },
      },
    ]);
  });

  it("runs /implement elsewhere on the thread its first word names", async () => {
    await app().type("/implement tester pages faster");
    await app().press("Enter");
    expect(asked).toEqual([
      {
        method: "loop.request",
        params: { thread: "w2", kind: "implement", task: "pages faster" },
      },
    ]);
  });

  it("shows the core's message for /loop that does nothing", async () => {
    await app().type("/loop implement");
    await app().press("Enter");
    expect(asked).toEqual([]);
    expect(app().find("[role=alert]").textContent).toContain(
      "use /implement <task> or /review",
    );
  });
});
