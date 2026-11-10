import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopEvent, Json, Thread } from "../shared/types";
import {
  FakeEvent,
  installFakeDOM,
  press,
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

function fakeHost() {
  const listeners = new Set<(event: DesktopEvent) => void>();
  const secure = new Map<string, string>();
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
      if (method === "thread.transcript") return { events: [], more: false };
      if (method === "desktop.slash_commands") return [];
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
    configRead: async () => ({ content: "", hash: "", path: "", settings: {} }),
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
  };
  return host;
}

type Mounted = Awaited<ReturnType<typeof mount>>;
async function mount() {
  const document = installFakeDOM() as FakeDocument;
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  const host = fakeHost();
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
  /** Delivers a workspace's live view, as the hub does once it is connected. */
  const live = async (workspace: string) => {
    await act(async () => {
      host.emit({ type: "connection", connected: true } as DesktopEvent);
      host.emit({
        type: "state",
        state: {
          threads: views[workspace],
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
  const find = (selector: string, text?: string) => {
    const node = document
      .querySelectorAll(selector)
      .find((node) => text === undefined || node.textContent.includes(text));
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
    find,
    click,
    box,
    type: (text: string) => act(async () => typeInto(box(), text)),
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
