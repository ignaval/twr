import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  DesktopEvent,
  HubEvent,
  InboxSource,
  Json,
  Thread,
} from "../shared/types";
import type { NativeVoiceEvent, VoiceCaptureStatus } from "../shared/voice";
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
  voice?: boolean;
  /** This computer's speaker bridge, which shows the header's read-aloud control. */
  speaker?: boolean;
  voiceStatus?: () => Promise<VoiceCaptureStatus>;
  voiceSetup?: () => Promise<VoiceCaptureStatus>;
  inboxSources?: InboxSource[];
};

function fakeHost(options: HostOptions = {}) {
  const listeners = new Set<(event: DesktopEvent) => void>();
  const secure = new Map<string, string>(Object.entries(options.secure || {}));
  const sent: { method: string; params: Json }[] = [];
  const voiceListeners = new Set<(event: NativeVoiceEvent) => void>();
  const voiceOps: string[] = [];
  const nativeVoice = {
    status:
      options.voiceStatus || (async () => ({ available: true, local: true })),
    start: async ({ mode }: { mode: string }) => {
      voiceOps.push("start:" + mode);
      if (mode === "push")
        voiceListeners.forEach((listener) => listener({ type: "speechStart" }));
    },
    stop: async () => {
      voiceOps.push("stop");
    },
    cancel: async () => {
      voiceOps.push("cancel");
    },
    onEvent: (callback: (event: NativeVoiceEvent) => void) => {
      voiceListeners.add(callback);
      return () => {
        voiceListeners.delete(callback);
      };
    },
  };
  const host = {
    sent,
    nativeVoice: options.voice ? nativeVoice : undefined,
    voiceSpeaker: options.speaker
      ? async () => ({
          device: "computer",
          running: true,
          active: false,
          name: "desktop",
          playing: false,
          paused: false,
          queued: 0,
        })
      : undefined,
    voiceSetup: options.voiceSetup,
    voiceOps,
    voiceEvent: (event: NativeVoiceEvent) =>
      voiceListeners.forEach((listener) => listener(event)),
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
    inboxSources: async () => options.inboxSources || [],
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
          path: "/w/.twr-uploads/u/" + file.name,
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
    mobile?: boolean;
  } = {},
) {
  const document = installFakeDOM() as FakeDocument;
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  for (const [key, value] of Object.entries(options.local || {}))
    localStorage.setItem(key, value);
  if (options.hub) document.documentElement.dataset.hub = options.hub;
  if (options.mobile) document.documentElement.dataset.mobile = "true";
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
    /** Opens a worker's conversation with one click on its sidebar card. */
    talkTo: async (name: string) => {
      await click(find(".sidebar .thread-card", name));
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

describe("the latest user message above conversation history", () => {
  afterEach(async () => {
    await mounted.unmount();
  });
  const start = async (options: HostOptions = {}) => {
    vi.resetModules();
    mounted = await mount(options);
  };
  const prompt = (
    turn: number,
    text: string,
    from = "user",
    thread = "orch",
  ): HubEvent => ({
    id: `${thread}:${turn}`,
    thread,
    seq: turn * 10,
    turn,
    ts: "2026-11-17T12:00:00Z",
    kind: "turn.started",
    data: { from, prompt: text },
  });
  const preview = () => app().find(".latest-user-preview").textContent;
  const deferredPage = () => {
    let resolve!: (page: Json) => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<Json>((yes, no) => {
      resolve = yes;
      reject = no;
    });
    return { promise, resolve, reject };
  };

  it("shows only the latest user's first line and ignores drafts and automatic turns", async () => {
    await start({
      events: [
        prompt(1, "Older request"),
        prompt(2, "Latest request\r\nA second line"),
        prompt(3, "Automatic prompt", "orchestrator"),
      ],
    });
    expect(preview()).toBe("Latest request");
    expect(
      app().find(".latest-user-message button").getAttribute("aria-label"),
    ).toBe("Jump to your previous message: Latest request");
    expect(app().all(".conversation-scroll .latest-user-message")).toHaveLength(
      0,
    );
    await app().type("Unsent draft");
    expect(preview()).toBe("Latest request");
    await app().connection(false);
    expect(preview()).toBe("Latest request");
  });

  it("updates on a live user message, but keeps it through agent replies and prepended history", async () => {
    let subscription = "";
    await start({
      rpc: (method, params) => {
        if (method === "desktop.subscribe") subscription = params.subscription;
        if (method === "thread.transcript")
          return Promise.resolve({
            events: [
              prompt(
                params.turn || 2,
                "Request " + (params.turn || 2) + "x".repeat(3000),
              ),
            ],
            turn: params.turn || 2,
            more: false,
          });
      },
    });
    await app().click(app().find(".load-earlier"));
    expect(preview()).toContain("Request 2");
    await app().act(async () => {
      app().host.emit({
        type: "thread",
        subscription,
        event: prompt(3, "New request\nDetails"),
      });
    });
    await app().wait(60);
    expect(preview()).toBe("New request");
    await app().act(async () => {
      app().host.emit({
        type: "thread",
        subscription,
        event: {
          ...prompt(4, ""),
          kind: "agent.text",
          data: { text: "Reply", delta: true },
        },
      });
    });
    await app().wait(60);
    expect(preview()).toBe("New request");
  });

  it("loads beyond three automatic turns and all pages to find the latest user", async () => {
    const turns: number[] = [];
    await start({
      rpc: (method, params) => {
        if (method !== "thread.transcript") return;
        const turn = params.turn || 8;
        turns.push(turn);
        const paged = turn === 4 && !params.since_seq;
        return Promise.resolve({
          events: params.since_seq
            ? []
            : [
                prompt(
                  turn,
                  turn === 3
                    ? "Actual latest request\nDetails"
                    : "x".repeat(3100),
                  turn === 3 ? "user" : "orchestrator",
                ),
              ],
          turn,
          more: paged,
          next: turn * 10,
        });
      },
    });
    expect(turns).toEqual([8, 7, 6, 5, 4, 4, 3]);
    expect(preview()).toBe("Actual latest request");
  });

  it("subscribes and accepts live messages while an automatic worker's older history is pending", async () => {
    const older = deferredPage();
    const calls: string[] = [];
    let subscription = "";
    await start({
      rpc: (method, params) => {
        if (method === "desktop.subscribe") {
          calls.push("subscribe");
          subscription = params.subscription;
        }
        if (method !== "thread.transcript") return;
        calls.push(String(params.turn || "recent"));
        return params.turn
          ? older.promise
          : Promise.resolve({
              events: [
                prompt(100, "Automatic " + "x".repeat(3100), "orchestrator"),
              ],
              turn: 100,
              more: false,
              next: 1000,
            });
      },
    });
    expect(calls).toEqual(["recent", "subscribe", "99"]);
    expect(app().all(".transcript-loading")).toHaveLength(0);
    expect(
      app().find(".conversation-scroll").getAttribute("aria-busy"),
    ).toBeNull();
    expect(app().find(".latest-user-empty").textContent).toBe(
      "Loading your previous message…",
    );
    await app().act(async () =>
      app().host.emit({
        type: "thread",
        subscription,
        event: prompt(101, "Live user request"),
      }),
    );
    await app().wait(60);
    expect(preview()).toBe("Live user request");
    await app().act(async () =>
      older.resolve({
        events: [prompt(99, "Older automatic task", "orchestrator")],
        more: false,
      }),
    );
    await app().settle();
    expect(preview()).toBe("Live user request");
    expect(calls).toEqual(["recent", "subscribe", "99"]);
  });

  it("retains recent history and live delivery when an older page fails, and allows retry", async () => {
    let failOlder = true;
    let subscription = "";
    const turns: number[] = [];
    await start({
      rpc: (method, params) => {
        if (method === "desktop.subscribe") subscription = params.subscription;
        if (method !== "thread.transcript") return;
        const turn = params.turn || 100;
        turns.push(turn);
        if (!params.turn)
          return Promise.resolve({
            events: [prompt(100, "Recent user request")],
            turn: 100,
            more: false,
          });
        if (params.since_seq && failOlder)
          return Promise.reject(new Error("Older page unavailable"));
        return Promise.resolve({
          events: params.since_seq
            ? []
            : [prompt(99, "x".repeat(3100), "orchestrator")],
          more: !params.since_seq,
          next: 990,
        });
      },
    });
    expect(turns).toEqual([100, 99, 99]);
    expect(preview()).toBe("Recent user request");
    expect(
      app().find(".conversation-scroll .error-banner").textContent,
    ).toContain("Older page unavailable");
    expect(app().all(".transcript-loading")).toHaveLength(0);
    await app().act(async () =>
      app().host.emit({
        type: "thread",
        subscription,
        event: prompt(101, "Live request after failure"),
      }),
    );
    await app().wait(60);
    expect(preview()).toBe("Live request after failure");
    expect(turns).toEqual([100, 99, 99]); // No automatic retry loop on live updates.
    failOlder = false;
    await app().click(app().find(".load-earlier"));
    expect(turns).toEqual([100, 99, 99, 99, 99]);
    expect(app().all(".conversation-scroll .error-banner")).toHaveLength(0);
    expect(preview()).toBe("Live request after failure");
  });

  it.each(["page", "error"])(
    "ignores an old conversation's delayed %s without disturbing the new lookup",
    async (result) => {
      const oldPage = deferredPage();
      const workerPage = deferredPage();
      const calls: string[] = [];
      await start({
        rpc: (method, params) => {
          if (method !== "thread.transcript") return;
          calls.push(
            `${params.thread}:${params.turn || "recent"}:${params.since_seq || 0}`,
          );
          if (params.turn)
            return params.thread === "orch"
              ? oldPage.promise
              : workerPage.promise;
          const turn = params.thread === "orch" ? 100 : 10;
          return Promise.resolve({
            events: [
              prompt(turn, "x".repeat(3100), "orchestrator", params.thread),
            ],
            turn,
            more: false,
          });
        },
      });
      await app().talkTo("builder");
      expect(calls).toEqual([
        "orch:recent:0",
        "orch:99:0",
        "w1:recent:0",
        "w1:9:0",
      ]);
      await app().act(async () => {
        if (result === "error")
          oldPage.reject(new Error("Stale history error"));
        else
          oldPage.resolve({
            events: [prompt(99, "Stale user request")],
            more: true,
            next: 991,
          });
      });
      await app().settle();
      expect(calls).toHaveLength(4); // No next page or duplicate worker request.
      expect(app().all(".latest-user-preview")).toHaveLength(0);
      expect(app().all(".conversation-scroll .error-banner")).toHaveLength(0);
      expect(app().find(".load-earlier").disabled).toBe(true);
      await app().act(async () =>
        workerPage.resolve({
          events: [prompt(9, "Worker's latest request", "user", "w1")],
          more: false,
        }),
      );
      await app().settle();
      expect(preview()).toBe("Worker's latest request");
      expect(app().find(".load-earlier").disabled).toBe(false);
    },
  );

  it("keeps short-conversation context bounded to three turns after subscribing", async () => {
    const calls: string[] = [];
    await start({
      rpc: (method, params) => {
        if (method === "desktop.subscribe") calls.push("subscribe");
        if (method !== "thread.transcript") return;
        const turn = params.turn || 8;
        calls.push(String(turn));
        return Promise.resolve({
          events: [
            prompt(turn, "Short context", turn === 8 ? "user" : "orchestrator"),
          ],
          turn,
          more: false,
        });
      },
    });
    expect(calls).toEqual(["8", "subscribe", "7", "6", "5"]);
  });

  it("clears the previous conversation's line while a thread deep dive loads", async () => {
    let finish: (value: Json) => void = () => {};
    await start({
      rpc: (method, params) => {
        if (method !== "thread.transcript") return;
        return params.thread === "orch"
          ? Promise.resolve({
              events: [prompt(1, "Orchestrator request")],
              more: false,
            })
          : new Promise((resolve) => {
              finish = resolve;
            });
      },
    });
    expect(preview()).toBe("Orchestrator request");
    await app().talkTo("builder");
    expect(app().all(".latest-user-preview")).toHaveLength(0);
    expect(app().find(".latest-user-empty").textContent).toBe(
      "Loading your previous message…",
    );
    await app().act(async () =>
      finish({
        events: [prompt(1, "Worker request", "user", "w1")],
        more: false,
      }),
    );
    await app().settle();
    expect(preview()).toBe("Worker request");
  });

  it("has no jump action when the complete history has no user message", async () => {
    await start({ events: [prompt(1, "Automatic", "orchestrator")] });
    expect(app().all(".latest-user-message button")).toHaveLength(0);
    expect(app().find(".latest-user-empty").textContent).toBe(
      "No messages from you yet",
    );
  });

  it("keeps a message with a blank first line reachable", async () => {
    await start({ events: [prompt(1, "\nDetails on the next line")] });
    expect(preview()).toBe("Blank first line");
    expect(
      app().find(".latest-user-message button").getAttribute("title"),
    ).toBe("Blank first line");
  });
});

describe("dictation in the shared desktop and mobile composer", () => {
  afterEach(async () => {
    await mounted.unmount();
  });
  const start = async (options: Parameters<typeof mount>[0] = {}) => {
    vi.resetModules();
    mounted = await mount({ ...options, voice: true });
  };
  const voiceEvent = async (event: NativeVoiceEvent) => {
    await app().act(async () => app().host.voiceEvent(event));
    await app().settle();
  };
  const mic = () => app().find("[data-voice-push]");
  const dictate = async (text: string) => {
    // Keyboard activation exercises the same microphone button without pointer events.
    await app().click(mic(), { detail: 0 });
    await app().click(mic(), { detail: 0 });
    await voiceEvent({ type: "final", text });
  };
  const send = async () => {
    await app().act(async () => {
      app()
        .find("form.composer")
        .dispatchEvent(
          new FakeEvent("submit", { bubbles: true, cancelable: true }),
        );
    });
    await app().settle();
  };

  it.each([false, true])(
    "appends to the latest draft, focuses it, and sends only explicitly (mobile=%s)",
    async (mobile) => {
      await start({ mobile });
      await app().type("Typed first.");
      await app().click(mic(), { detail: 0 });
      await app().type(" Still typing.");
      await app().click(mic(), { detail: 0 });
      await voiceEvent({ type: "partial", text: "Spoken" });
      expect(app().box().value).toBe("Typed first. Still typing.");
      await voiceEvent({ type: "final", text: "Spoken last." });
      expect(app().host.sent).toEqual([]);
      expect(app().all(".voice-bar")).toHaveLength(0);
      expect(app().all("[role=dialog], [role=alertdialog]")).toHaveLength(0);
      expect(app().box().value).toBe("Typed first. Still typing. Spoken last.");
      expect(app().document.activeElement).toBe(app().box());
      await app().type(" Edited.");
      await send();
      expect(app().host.sent).toEqual([
        {
          method: "say",
          params: {
            message: "Typed first. Still typing. Spoken last. Edited.",
            seen_gseq: 1,
            to: "orchestrator",
            voice: true,
          },
        },
      ]);
      expect(app().box().value).toBe("");
    },
  );
  it.each([false, true])(
    "keeps active and error feedback inline and removes it after completion or dismissal (mobile=%s)",
    async (mobile) => {
      await start({ mobile });
      expect(app().all(".voice-bar")).toHaveLength(0);
      await app().click(mic(), { detail: 0 });
      expect(
        app().find(".composer-voice-status .voice-bar").textContent,
      ).toContain("Hearing you");
      expect(app().all(".composer-actions .voice-bar")).toHaveLength(0);
      expect(app().all("[role=dialog], [role=alertdialog]")).toHaveLength(0);
      await app().click(mic(), { detail: 0 });
      expect(
        app().find(".composer-voice-status .voice-bar").textContent,
      ).toContain("Transcribing");
      await voiceEvent({ type: "final", text: "Editable words" });
      expect(app().all(".voice-bar")).toHaveLength(0);
      expect(app().find("form.composer").textContent).not.toMatch(
        /voice (?:is )?ready/i,
      );
      await app().click(mic(), { detail: 0 });
      await voiceEvent({ type: "error", message: "Microphone unavailable" });
      expect(
        app().find(".composer-voice-status .voice-bar").textContent,
      ).toContain("Microphone unavailable");
      expect(app().box().value).toBe("Editable words");
      await app().click(app().find(".voice-bar button", "Dismiss"));
      expect(app().all(".voice-bar")).toHaveLength(0);
      expect(app().box().value).toBe("Editable words");
    },
  );
  it("removes model setup feedback after download without a ready dialog or acknowledgement", async () => {
    let finish!: (status: VoiceCaptureStatus) => void;
    await start({
      voiceStatus: async () => ({
        available: false,
        local: true,
        setup: true,
        problem: "No model",
      }),
      voiceSetup: () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    });
    await app().click(mic(), { detail: 0 });
    expect(
      app().find(".composer-voice-status .voice-bar").textContent,
    ).toContain("Voice needs a speech model");
    await app().click(app().find(".voice-bar button", "Download model"));
    expect(app().find(".voice-bar").textContent).toContain(
      "Downloading the speech model",
    );
    await app().act(async () => finish({ available: true, local: true }));
    await app().settle();
    expect(app().all(".voice-bar")).toHaveLength(0);
    expect(app().all("[role=dialog], [role=alertdialog]")).toHaveLength(0);
    expect(app().find("form.composer").textContent).not.toMatch(
      /voice (?:is )?ready/i,
    );
    expect(app().host.sent).toEqual([]);
  });
  it("keeps failed model setup inline with retry and dismiss actions", async () => {
    await start({
      voiceStatus: async () => ({
        available: false,
        local: true,
        setup: true,
        problem: "No model",
      }),
      voiceSetup: async () => {
        throw new Error("Download failed");
      },
    });
    await app().click(mic(), { detail: 0 });
    await app().click(app().find(".voice-bar button", "Download model"));
    expect(
      app().find(".composer-voice-status .voice-bar").textContent,
    ).toContain("Download failed");
    expect(app().all("[role=dialog], [role=alertdialog]")).toHaveLength(0);
    expect(app().find(".voice-bar button", "Download model")).toBeDefined();
    await app().click(app().find(".voice-bar button", "Dismiss"));
    expect(app().all(".voice-bar")).toHaveLength(0);
  });
  it("supports hold-to-talk, the keyboard shortcut, and an end-of-draft caret", async () => {
    await start();
    await app().type("Typed");
    const pointer = async (type: string) => {
      await app().act(async () => {
        mic().dispatchEvent(
          new FakeEvent(type, { bubbles: true, pointerId: 1, button: 0 }),
        );
      });
      await app().settle();
    };
    await pointer("pointerdown");
    await app().wait(360);
    await pointer("pointerup");
    expect(app().host.voiceOps).toContain("stop");
    await voiceEvent({ type: "final", text: "by holding" });
    expect(app().box().value).toBe("Typed by holding");
    expect(app().box().selectionStart).toBe(app().box().value.length);
    const shortcut = async () => {
      await app().act(async () => {
        window.dispatchEvent(
          new FakeEvent("keydown", {
            code: "Space",
            ctrlKey: true,
            shiftKey: true,
            bubbles: true,
            cancelable: true,
          }) as unknown as Event,
        );
      });
      await app().settle();
    };
    await shortcut();
    await shortcut();
    await voiceEvent({ type: "final", text: "by shortcut" });
    expect(app().box().value).toBe("Typed by holding by shortcut");
    expect(app().host.sent).toEqual([]);
  });
  it("guards slash suggestions and Send as implementation loop against unreviewed dictation", async () => {
    const methods: string[] = [];
    await start({
      rpc: (method) => {
        methods.push(method);
        return undefined;
      },
    });
    await dictate("/job");
    methods.length = 0;
    await app().press("ArrowDown");
    await app().press("Enter");
    expect(methods).not.toContain("desktop.parse_slash");
    expect(app().find(".dialog").textContent).toContain(
      "Confirm spoken message",
    );
    await app().click(app().find(".dialog button", "Cancel"));
    await app().talkTo("builder");
    await dictate("Implement a task");
    await app().click(app().find(".send-caret"));
    await app().click(
      app().find(".send-loop-panel button", "Send as implementation loop"),
    );
    expect(methods).not.toContain("thread.implement");
    expect(app().find(".dialog").textContent).toContain(
      "Confirm spoken message",
    );
  });
  it("keeps dictation at the selected worker; hands-free still sends to the orchestrator", async () => {
    await start();
    await app().talkTo("builder");
    await dictate("Check the tests");
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
    expect(app().host.sent).toEqual([]);
    await send();
    expect(app().host.sent).toEqual([
      {
        method: "thread.send",
        params: { thread: "w1", prompt: "Check the tests" },
      },
    ]);
    await app().type("Keep this worker draft");
    await app().click(app().find("[data-voice-hands-free]"));
    await voiceEvent({ type: "speechStart" });
    await voiceEvent({ type: "final", text: "How is the workspace?" });
    expect(app().host.sent.at(-1)).toMatchObject({
      method: "say",
      params: {
        to: "orchestrator",
        voice: true,
        message: "How is the workspace?",
      },
    });
    expect(app().box().value).toBe("Keep this worker draft");
    expect(
      app().find(".composer-voice-status .voice-bar").textContent,
    ).toContain("Listening");
  });
  it("retains spoken provenance through edits and encrypted draft reload, then requires explicit review of approvals", async () => {
    const calls: Json[] = [];
    const rpc: HostOptions["rpc"] = (method, params) => {
      if (method === "say") {
        calls.push(params);
        return Promise.resolve(
          params.voice ? { action: "confirm" } : { action: "sent" },
        );
      }
    };
    await start({ rpc });
    await dictate("allow it");
    await app().replace("@builder yes");
    await app().wait(400);
    const secure = Object.fromEntries(app().host.secure);
    await app().unmount();
    await start({ rpc, secure });
    expect(app().box().value).toBe("@builder yes");
    await send();
    expect(calls).toHaveLength(1);
    expect(calls[0].voice).toBe(true);
    expect(app().find(".dialog").textContent).toContain(
      "Confirm spoken message",
    );
    await app().click(app().find(".dialog button", "Review in message box"));
    expect(calls).toHaveLength(1);
    expect(app().box().value).toBe("@builder yes");
    await send();
    expect(calls).toHaveLength(2);
    expect(calls[1].voice).toBeUndefined();
    expect(app().box().value).toBe("");
  });
  it.each(["/jobs", "!echo hello"])(
    "holds dictated composer commands for review: %s",
    async (command) => {
      const methods: string[] = [];
      await start({
        rpc: (method) => {
          methods.push(method);
          return undefined;
        },
      });
      await dictate(command);
      methods.length = 0;
      await send();
      expect(methods).not.toContain("desktop.parse_slash");
      expect(methods).not.toContain("shell.run");
      expect(app().find(".dialog").textContent).toContain(
        "Confirm spoken message",
      );
      expect(app().box().value).toBe(command);
    },
  );
  it("drops late results on conversation switches, cancellation, disconnect and errors", async () => {
    await start();
    await app().type("Keep orchestrator draft");
    await dictate("");
    expect(app().box().value).toBe("Keep orchestrator draft");
    await app().click(mic(), { detail: 0 });
    await app().talkTo("builder");
    await voiceEvent({ type: "final", text: "old conversation" });
    expect(app().box().value).toBe("");
    await app().type("Keep worker draft");
    await app().click(mic(), { detail: 0 });
    await app().click(app().find("button", "Cancel dictation"));
    await voiceEvent({ type: "final", text: "cancelled" });
    expect(app().box().value).toBe("Keep worker draft");
    await app().click(mic(), { detail: 0 });
    await voiceEvent({ type: "error", message: "Recognition failed" });
    await voiceEvent({ type: "final", text: "late after error" });
    expect(app().box().value).toBe("Keep worker draft");
    await app().click(mic(), { detail: 0 });
    await app().connection(false);
    await voiceEvent({ type: "final", text: "disconnected" });
    expect(app().box().value).toBe("Keep worker draft");
    expect(app().host.sent).toEqual([]);
  });
  it("keeps dictated text out of the offline delivery queue", async () => {
    await start();
    const deliver = vi.fn(async () => ({ ok: true }));
    app().host.sendMessage = deliver;
    await dictate("Check the tests");
    await app().connection(false);
    await send();
    expect(deliver).not.toHaveBeenCalled();
    expect(app().box().value).toBe("Check the tests");
    await app().live("default");
    await send();
    expect(deliver).not.toHaveBeenCalled();
    expect(app().host.sent[0]).toMatchObject({
      method: "say",
      params: { voice: true },
    });
  });
});

describe("thread navigation", () => {
  beforeEach(async () => {
    vi.resetModules();
    mounted = await mount();
  });
  afterEach(async () => {
    await mounted.unmount();
  });

  it("opens a sidebar conversation on one click and toggles its details without changing the draft", async () => {
    expect(app().all('[aria-label="Show thread details"]')).toHaveLength(0);
    await app().talkTo("builder");
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
    expect(app().all("#twr-inspector")).toHaveLength(0);
    expect(
      app()
        .find(".sidebar .thread-card", "builder")
        .getAttribute("aria-current"),
    ).toBe("location");
    await app().type("keep these words");
    const toggle = app().find('[aria-label="Show thread details"]');
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(toggle.getAttribute("aria-controls")).toBe("twr-inspector");
    await app().click(toggle);
    expect(app().find("#twr-inspector").getAttribute("aria-label")).toBe(
      "Thread details: builder",
    );
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    await app().click(app().find('[aria-label="Hide thread details"]'));
    expect(app().all("#twr-inspector")).toHaveLength(0);
    expect(app().box().value).toBe("keep these words");
    await app().click(toggle);
    await app().click(app().find('[aria-label="Close thread details"]'));
    expect(app().document.activeElement).toBe(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    await app().click(toggle);
    await app().talkTo("tester");
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message tester directly",
    );
    expect(app().all("#twr-inspector")).toHaveLength(0);
    await app().click(app().find('[aria-label="Show thread details"]'));
    expect(app().find("#twr-inspector").getAttribute("aria-label")).toBe(
      "Thread details: tester",
    );
  });

  it("opens a conversation from the Threads grid on one click", async () => {
    await app().click(app().find(".primary-nav button", "Thread grid"));
    await app().click(app().find(".thread-grid .thread-card", "builder"));
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
    expect(app().all(".thread-grid")).toHaveLength(0);
    expect(app().all("#twr-inspector")).toHaveLength(0);
  });

  it("keeps loop chip clicks on loop progress without changing the conversation", async () => {
    await app().live(
      "default",
      views.default.map((t) =>
        t.id === "w1" ? { ...t, loop_line: "Round 1" } : t,
      ),
    );
    await app().click(app().find(".sidebar .loop-chip"));
    expect(app().box().getAttribute("aria-label")).toBe("Message orchestrator");
    expect(app().find("#twr-inspector").getAttribute("aria-label")).toBe(
      "Thread details: builder",
    );
    await app().talkTo("tester");
    await app().click(app().find(".sidebar .loop-chip"));
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message tester directly",
    );
    await app().click(app().find('[aria-label="Show thread details"]'));
    expect(app().find("#twr-inspector").getAttribute("aria-label")).toBe(
      "Thread details: tester",
    );
  });

  it("opens another workspace's thread conversation with one click", async () => {
    await mounted.unmount();
    const remote = thread({ id: "remote", name: "remote-builder" });
    mounted = await mount({
      inboxSources: [
        {
          hubId: "composer-test",
          hubName: "Test hub",
          workspace: "other",
          threads: [remote],
          attention: [],
        },
      ],
    });
    await app().click(
      app().find("[data-workspace=other] .workspace-thread", "remote-builder"),
    );
    await app().live("other", [...views.other, remote]);
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message remote-builder directly",
    );
    expect(app().all("#twr-inspector")).toHaveLength(0);
  });

  it("preserves mobile's details-first navigation and Enter to talk", async () => {
    await mounted.unmount();
    mounted = await mount({ mobile: true });
    await app().click(app().find(".sidebar .thread-card", "builder"));
    expect(app().box().getAttribute("aria-label")).toBe("Message orchestrator");
    expect(app().find("#twr-inspector").getAttribute("aria-label")).toBe(
      "Thread details: builder",
    );
    const card = app().find(".sidebar .thread-card", "builder");
    expect(card.getAttribute("aria-describedby")).toBe("twr-thread-open-hint");
    await app().act(async () => {
      press(card, "Enter");
    });
    await app().settle();
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
    expect(app().all('[aria-label="Show thread details"]')).toHaveLength(0);
  });
});

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

  it("keeps one of each header control across workspace switches", async () => {
    await mounted.unmount();
    mounted = await mount({ speaker: true });
    const count = (label: string) =>
      app().all(`.conversation-actions [aria-label="${label}"]`).length;
    const readAloud =
      "Automatically read orchestrator replies aloud on this computer";
    const before = [count(readAloud), count("Usage limits")];
    await app().switchTo("other");
    await app().switchTo("default");
    await app().switchTo("other");
    expect([count(readAloud), count("Usage limits")]).toEqual(before);
    expect(before).toEqual([1, 1]);
  });

  it("closes the /usage panel when you switch workspaces", async () => {
    await app().type("/usage");
    await app().press("Enter");
    expect(app().all(".usage-panel")).toHaveLength(1);
    await app().switchTo("other");
    expect(app().all(".usage-panel")).toHaveLength(0);
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

  it("takes a suggestion that starts with / as a message, escaped", async () => {
    await start({
      ...suggesting,
      rpc: (method: string) =>
        method === "suggest.next"
          ? Promise.resolve({ suggestion: "/usage is what I'd check" })
          : undefined,
    });
    for (let i = 0; i < 40; i++) {
      if (app().box().getAttribute("placeholder")?.startsWith("/usage")) break;
      await app().wait(100);
    }
    await app().wait(SUGGEST_SETTLE_MS + 50);
    await app().press("Tab");
    expect(app().box().value).toBe("//usage is what I'd check");
    await app().press("Enter");
    expect(app().all(".usage-panel")).toHaveLength(0);
    expect(app().host.sent[0].params).toMatchObject({
      message: "/usage is what I'd check",
      suggested: true,
    });
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

/* Attached files (docs/attachments.md): they show inside the message box, never as paths, and
 * a message names them (`Uploaded file: <name>` and the path) only as it is sent. */
describe("attached files", () => {
  afterEach(async () => {
    await mounted.unmount();
  });
  const start = async (options: Parameters<typeof mount>[0] = {}) => {
    vi.resetModules();
    mounted = await mount({ uploads: true, ...options });
  };
  /** Picks a file with the paperclip, and lets its upload finish. */
  const attach = async (name = "notes.txt", type = "text/plain") => {
    const input = app().find(".attachment-button input") as FakeElement & {
      files: unknown[];
    };
    input.files = [
      {
        name,
        size: 5,
        type,
        slice: () => ({ text: async () => "hello" }),
      },
    ];
    await app().act(async () => {
      input.dispatchEvent(new FakeEvent("change", { bubbles: true }));
    });
    await app().wait(50);
  };
  const chips = () =>
    app()
      .all(".composer-attachments li")
      .map((li) => li.textContent);
  /** Every text and attribute of the message box: where a path must never show. */
  const shown = () =>
    app()
      .all("form.composer, form.composer *")
      .flatMap((node) => [
        ...node.attributes
          .filter((a) => a.name !== "value")
          .map((a) => a.value),
      ])
      .concat(app().find("form.composer").textContent, app().box().value)
      .join("\n");
  const sendButton = () => app().find(".send-button");
  /** What the device saved of drafts and the outbox. */
  const pending = () =>
    JSON.parse(app().host.secure.get("offline-pending:composer-test") || "{}");
  /** Quits and starts again on this device's saved storage. */
  const restart = async (options: Parameters<typeof mount>[0] = {}) => {
    const secure = Object.fromEntries(app().host.secure);
    await app().unmount();
    await start({ secure, ...options });
  };
  const submit = async () => {
    await app().act(async () => {
      app()
        .find("form.composer")
        .dispatchEvent(
          new FakeEvent("submit", { bubbles: true, cancelable: true }),
        );
    });
    await app().settle();
  };

  it("shows a file inside the box, without its path, and keeps it out of the draft", async () => {
    await start();
    await app().type("see");
    await attach();
    expect(app().box().value).toBe("see");
    expect(chips()).toHaveLength(1);
    expect(chips()[0]).toContain("notes.txt");
    expect(shown()).not.toContain(".twr-uploads");
    // The old full-width bar is gone.
    expect(app().all(".attachment-manager, .attachment-item")).toHaveLength(0);
    // The draft saved on the device is the text alone; the file is saved beside it.
    await app().wait(450);
    expect(pending().drafts).toEqual({ "default:orchestrator": "see" });
    expect(pending().draftFiles).toEqual({
      "default:orchestrator": [
        { name: "notes.txt", path: "/w/.twr-uploads/u/notes.txt" },
      ],
    });
  });

  it("names the files after the text it sends, then clears them", async () => {
    await start();
    await app().type("see");
    await attach();
    await app().press("Enter");
    expect(app().host.sent).toEqual([
      {
        method: "say",
        params: expect.objectContaining({
          message: "see\nUploaded file: notes.txt\n/w/.twr-uploads/u/notes.txt",
        }),
      },
    ]);
    expect(app().box().value).toBe("");
    expect(chips()).toHaveLength(0);
  });

  it("sends files with no text, to the conversation they belong to", async () => {
    await start();
    await app().talkTo("builder");
    await attach("a.txt");
    expect(sendButton().disabled).toBe(false);
    await app().talkTo("tester");
    expect(chips()).toHaveLength(0);
    await app().talkTo("builder");
    expect(chips()).toHaveLength(1);
    await submit();
    expect(app().host.sent).toEqual([
      {
        method: "thread.send",
        params: {
          thread: "w1",
          prompt: "Uploaded file: a.txt\n/w/.twr-uploads/u/a.txt",
        },
      },
    ]);
    expect(chips()).toHaveLength(0);
  });

  it("waits, with Send disabled and saying why, while a file uploads", async () => {
    await start();
    let finish!: () => void;
    app().host.uploadFile = (async (file: { name: string }) => {
      await new Promise<void>((resolve) => (finish = resolve));
      return { path: "/w/.twr-uploads/u/" + file.name };
    }) as any;
    await app().type("soon");
    await attach();
    expect(sendButton().disabled).toBe(true);
    expect(sendButton().getAttribute("aria-describedby")).toBe(
      "twr-attachment-wait",
    );
    expect(app().find("#twr-attachment-wait").textContent).toContain(
      "finish uploading",
    );
    await app().press("Enter");
    expect(app().host.sent).toEqual([]);
    expect(app().box().value).toBe("soon");
    await app().act(async () => finish());
    await app().wait(50);
    expect(sendButton().disabled).toBe(false);
    await app().press("Enter");
    expect(app().host.sent[0].params.message).toBe(
      "soon\nUploaded file: notes.txt\n/w/.twr-uploads/u/notes.txt",
    );
  });

  it("attaches nothing to a composer command or a shell line", async () => {
    await start();
    await attach();
    await app().type("!ls");
    await app().press("Enter");
    expect(app().host.sent).toEqual([]);
    expect(chips()).toHaveLength(1);
    await app().replace("now");
    await app().press("Enter");
    expect(app().host.sent[0].params.message).toBe(
      "now\nUploaded file: notes.txt\n/w/.twr-uploads/u/notes.txt",
    );
  });

  it("saves the paths with a message kept for later, and shows only the names", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    await attach();
    await app().connection(false);
    await app().type("later");
    await app().press("Enter");
    expect(app().host.sent).toHaveLength(0);
    expect(chips()).toHaveLength(0);
    const outbox = app().find(".outbox-message");
    expect(outbox.textContent).toContain("later");
    expect(outbox.textContent).toContain("notes.txt");
    expect(outbox.textContent).not.toContain(".twr-uploads");
    await app().wait(450);
    expect([...app().host.secure.values()].join("\n")).toContain(
      "later\\nUploaded file: notes.txt\\n/w/.twr-uploads/u/notes.txt",
    );
    // Moved back to the draft: the text to the box, the file to the box's files.
    await app().click(
      app().find(".outbox-message button", "Move to original draft"),
    );
    expect(app().box().value).toBe("later");
    expect(chips()).toHaveLength(1);
    expect(shown()).not.toContain(".twr-uploads");
  });

  it("puts a queued message's files back on Take back, and shows them by name", async () => {
    await start();
    app().host.queues.w1 = [
      {
        ref: 7,
        prompt: "queued words\nUploaded file: a.png\n/w/.twr-uploads/q/a.png",
        mine: true,
      },
    ];
    await app().talkTo("builder");
    const queued = app().find(".queued-message");
    expect(queued.textContent).toContain("a.png");
    expect(queued.textContent).not.toContain("/w/.twr-uploads/q/a.png");
    await app().click(app().find(".queued-message button", "Take back"));
    expect(app().box().value).toBe("queued words");
    expect(chips()).toEqual([expect.stringContaining("a.png")]);
    await app().press("Enter");
    expect(app().host.sent.at(-1)).toEqual({
      method: "thread.send",
      params: {
        thread: "w1",
        prompt: "queued words\nUploaded file: a.png\n/w/.twr-uploads/q/a.png",
      },
    });
  });

  it("saves a taken-back / message with its file offline, as the message it is", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    app().host.queues.w1 = [
      {
        ref: 7,
        prompt:
          "/etc is the folder\nUploaded file: a.txt\n/w/.twr-uploads/q/a.txt",
        mine: true,
      },
    ];
    await app().talkTo("builder");
    await app().click(app().find(".queued-message button", "Take back"));
    expect(app().box().value).toBe("//etc is the folder");
    expect(chips()).toEqual([expect.stringContaining("a.txt")]);
    await app().connection(false);
    // Escaped, it is a message, not a command: it can be saved for later, its file with it.
    expect(sendButton().disabled).toBe(false);
    await app().press("Enter");
    expect(app().host.sent).toHaveLength(0);
    expect(app().box().value).toBe("");
    expect(chips()).toHaveLength(0);
    const outbox = app().find(".outbox-message");
    expect(outbox.textContent).toContain("/etc is the folder");
    expect(outbox.textContent).toContain("a.txt");
    await app().wait(450);
    expect([...app().host.secure.values()].join("\n")).toContain(
      "/etc is the folder\\nUploaded file: a.txt\\n/w/.twr-uploads/q/a.txt",
    );
    // A command itself still can't be saved offline.
    await app().type("/usage");
    expect(sendButton().disabled).toBe(true);
  });

  it("sends the orchestrator's files with a hands-free message", async () => {
    await start({ voice: true });
    await attach();
    await app().click(app().find("[data-voice-hands-free]"));
    await app().act(async () => app().host.voiceEvent({ type: "speechStart" }));
    await app().act(async () =>
      app().host.voiceEvent({ type: "final", text: "What is in it?" }),
    );
    await app().settle();
    expect(app().host.sent.at(-1)).toMatchObject({
      method: "say",
      params: {
        voice: true,
        message:
          "What is in it?\nUploaded file: notes.txt\n/w/.twr-uploads/u/notes.txt",
      },
    });
    expect(chips()).toHaveLength(0);
  });

  it("makes the files part of an implementation loop's task", async () => {
    const asked: Json[] = [];
    await start({
      rpc: (method, params) => {
        if (method === "loop.request") {
          asked.push(params);
          return Promise.resolve({ status: "started", loop: { line: "x" } });
        }
        if (method === "loop.get")
          return Promise.resolve({ started: null, id: "l1" });
        return undefined;
      },
    });
    await app().talkTo("builder");
    await attach("spec.md");
    await app().type("build this");
    await app().click(app().find(".send-caret"));
    await app().click(
      app().find(".send-loop-panel button", "Send as implementation loop"),
    );
    expect(asked).toEqual([
      {
        thread: "w1",
        kind: "implement",
        task: "build this\nUploaded file: spec.md\n/w/.twr-uploads/u/spec.md",
      },
    ]);
    expect(chips()).toHaveLength(0);
  });

  it("keeps saving drafts after a message of files alone goes to the outbox", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    await attach();
    await app().connection(false);
    await submit();
    expect(app().find(".outbox-message").textContent).toContain("notes.txt");
    expect(chips()).toHaveLength(0);
    await app().talkTo("builder");
    await app().type("second draft");
    await app().wait(450);
    expect(pending().drafts["default:w1"]).toBe("second draft");
    expect(pending().draftFiles).toEqual({});
  });

  it("keeps a draft's files over a restart, with text and alone", async () => {
    await start();
    await app().type("See attached");
    await attach();
    await app().talkTo("builder");
    await attach("a.txt");
    await app().wait(450);
    await restart();
    expect(app().box().value).toBe("See attached");
    expect(chips()).toEqual([expect.stringContaining("notes.txt")]);
    await app().talkTo("builder");
    expect(app().box().value).toBe("");
    expect(chips()).toEqual([expect.stringContaining("a.txt")]);
    await submit();
    expect(app().host.sent.at(-1)).toEqual({
      method: "thread.send",
      params: {
        thread: "w1",
        prompt: "Uploaded file: a.txt\n/w/.twr-uploads/u/a.txt",
      },
    });
  });

  it("saves a file the moment it is uploaded or removed, without typing's delay", async () => {
    await start();
    await attach();
    // Quit at once, well within the 350 ms the drafts wait after typing stops.
    await restart();
    expect(chips()).toEqual([expect.stringContaining("notes.txt")]);
    await app().click(app().find(".attachment-remove"));
    expect(chips()).toEqual([]);
    await restart();
    expect(chips()).toEqual([]);
  });

  it("saves what waits for its delay as the app goes away, or the page unloads", async () => {
    await start();
    await app().type("quick words");
    await attach();
    await app().type(" and a file");
    // The desktop window closing, or the phone app going to the background.
    const { saveNow } = await import("../shared/save-now");
    await app().act(() => saveNow());
    await restart();
    expect(app().box().value).toBe("quick words and a file");
    expect(chips()).toEqual([expect.stringContaining("notes.txt")]);
    await app().type(", then more");
    await app().act(async () => {
      window.dispatchEvent(new FakeEvent("pagehide") as unknown as Event);
    });
    await app().settle();
    await restart();
    expect(app().box().value).toBe("quick words and a file, then more");
  });

  it("writes nothing as the page unloads when every draft is saved already", async () => {
    await start();
    await app().type("saved words");
    await app().wait(450);
    // Written whole from memory, a needless save would undo what was stored since (an
    // outbox message another writer added, say).
    const write = app().host.secureWrite!;
    let writes = 0;
    app().host.secureWrite = async (key: string, value: string) => {
      writes += 1;
      return write(key, value);
    };
    await app().act(async () => {
      window.dispatchEvent(new FakeEvent("pagehide") as unknown as Event);
    });
    await app().settle();
    expect(writes).toBe(0);
    await app().type(" and more");
    await app().act(async () => {
      window.dispatchEvent(new FakeEvent("pagehide") as unknown as Event);
    });
    await app().settle();
    expect(writes).toBeGreaterThan(0);
  });

  it("keeps a file attached just before a send the outbox refuses, over a restart", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    await app().connection(false);
    await app().type("later");
    await app().wait(450);
    // The outbox refuses the message (it is full); every other save goes through.
    const write = app().host.secureWrite;
    app().host.secureWrite = async (key: string, value: string) => {
      if (JSON.parse(value).outbox?.length)
        throw new Error("Outbox is full (100 messages)");
      return write(key, value);
    };
    // Sent before the file's own save (350 ms after it uploaded) comes.
    await attach();
    await app().press("Enter");
    expect(app().all(".outbox-message")).toHaveLength(0);
    expect(app().box().value).toBe("later");
    expect(chips()).toEqual([expect.stringContaining("notes.txt")]);
    expect(pending().draftFiles).toEqual({
      "default:orchestrator": [
        { name: "notes.txt", path: "/w/.twr-uploads/u/notes.txt" },
      ],
    });
    app().host.secureWrite = write;
    await restart();
    expect(app().box().value).toBe("later");
    expect(chips()).toEqual([expect.stringContaining("notes.txt")]);
  });

  it("moves an older app's file lines out of a saved draft, into the box's files", async () => {
    await start();
    await app().type("x");
    await app().wait(450);
    // The previous composer wrote each uploaded file's pair into the draft itself.
    const record = pending();
    record.drafts["default:orchestrator"] =
      "See\nUploaded file: old.txt \n/w/.twr-uploads/9/old.txt \nmore\n```\nUploaded file: mock.txt\n/w/.twr-uploads/1/mock.txt\n```";
    app().host.secure.set(
      "offline-pending:composer-test",
      JSON.stringify(record),
    );
    await restart();
    expect(app().box().value).toBe(
      "See\nmore\n```\nUploaded file: mock.txt\n/w/.twr-uploads/1/mock.txt\n```",
    );
    expect(chips()).toEqual([expect.stringContaining("old.txt")]);
    expect(shown()).not.toContain("/w/.twr-uploads/9");
    await app().wait(450);
    expect(pending().draftFiles["default:orchestrator"]).toEqual([
      { name: "old.txt ", path: "/w/.twr-uploads/9/old.txt " },
    ]);
    // Sent, the file goes with the message as it was uploaded: its path exactly.
    await submit();
    expect(app().host.sent.at(-1)?.params.message).toBe(
      "See\nmore\n```\nUploaded file: mock.txt\n/w/.twr-uploads/1/mock.txt\n```\nUploaded file: old.txt\n/w/.twr-uploads/9/old.txt ",
    );
  });

  it("moves the old mobile app's file lines into the box's files", async () => {
    await start({
      hub: "h1",
      local: {
        "twr.mobile.drafts.h1": JSON.stringify({
          "default:orchestrator":
            "old words\nUploaded file: a.png\n/w/.twr-uploads/3/a.png",
        }),
      },
    });
    expect(app().box().value).toBe("old words");
    expect(chips()).toEqual([expect.stringContaining("a.png")]);
    expect(pending().drafts["default:orchestrator"]).toBe("old words");
    expect(pending().draftFiles["default:orchestrator"]).toEqual([
      { name: "a.png", path: "/w/.twr-uploads/3/a.png" },
    ]);
  });

  it("empties the box's files when offline storage is reset, and saves none of them", async () => {
    await start();
    await app().type("See attached");
    await attach();
    expect(chips()).toHaveLength(1);
    // As Settings' reset does: the storage is emptied (the file was saved as it uploaded),
    // then the app is told.
    app().host.secure.clear();
    await app().act(async () => {
      window.dispatchEvent(
        new FakeEvent("twr:offline-reset", {
          detail: { hostId: "composer-test" },
        }) as unknown as Event,
      );
    });
    await app().settle();
    expect(chips()).toEqual([]);
    await app().wait(450);
    const stored = [...app().host.secure.values()].join("\n");
    expect(stored).not.toContain("notes.txt");
    expect(stored).not.toContain("/w/.twr-uploads/u/");
  });

  it("keeps a taken-back message's files over a restart", async () => {
    await start();
    app().host.queues.w1 = [
      {
        ref: 7,
        prompt: "See attached\nUploaded file: a.png\n/w/.twr-uploads/q/a.png",
        mine: true,
      },
    ];
    await app().talkTo("builder");
    await app().click(app().find(".queued-message button", "Take back"));
    await app().wait(450);
    await restart();
    await app().talkTo("builder");
    expect(app().box().value).toBe("See attached");
    expect(chips()).toEqual([expect.stringContaining("a.png")]);
  });

  it("keeps a message's files over a restart once it is moved back from the outbox", async () => {
    await start();
    app().host.sendMessage = async () => ({ action: "sent" });
    await attach();
    await app().connection(false);
    await app().type("later");
    await app().press("Enter");
    await app().click(
      app().find(".outbox-message button", "Move to original draft"),
    );
    // Saved split at once, before the drafts' own save: a restart now loses nothing.
    expect(pending().drafts["default:orchestrator"]).toBe("later");
    expect(pending().draftFiles["default:orchestrator"]).toEqual([
      { name: "notes.txt", path: "/w/.twr-uploads/u/notes.txt" },
    ]);
    await restart();
    expect(app().box().value).toBe("later");
    expect(chips()).toEqual([expect.stringContaining("notes.txt")]);
  });

  it("keeps the orchestrator's files when a switch gives it a new thread", async () => {
    await start();
    const uploadedTo: string[] = [];
    let finish!: () => void;
    app().host.uploadFile = (async (file: { name: string }, thread: string) => {
      uploadedTo.push(thread);
      if (file.name === "late.txt")
        await new Promise<void>((resolve) => (finish = resolve));
      return { path: "/w/.twr-uploads/u/" + file.name };
    }) as any;
    await attach();
    await attach("late.txt");
    await app().type("draft");
    const switched = [
      thread({ id: "orch-new", name: "orchestrator", is_orchestrator: true }),
      ...views.default.filter((t) => !t.is_orchestrator),
    ];
    await app().live("default", switched);
    expect(app().box().value).toBe("draft");
    expect(chips()).toHaveLength(2);
    // The transfer that began goes on to the thread its receipt names.
    await app().act(async () => finish());
    await app().wait(50);
    expect(uploadedTo).toEqual(["orch", "orch"]);
    await app().press("Enter");
    expect(app().host.sent.at(-1)?.params.message).toBe(
      "draft\nUploaded file: notes.txt\n/w/.twr-uploads/u/notes.txt\nUploaded file: late.txt\n/w/.twr-uploads/u/late.txt",
    );
    expect(chips()).toHaveLength(0);
  });

  it("names a file in one message when hands-free speech and Send go at once", async () => {
    let deliver!: () => void;
    await start({
      voice: true,
      rpc: (method, params) => {
        if (method !== "say" || params.voice) return undefined;
        app().host.sent.push({ method, params });
        return new Promise((resolve) => {
          deliver = () => resolve({ action: "sent" });
        });
      },
    });
    await attach();
    await app().type("typed");
    await app().press("Enter");
    await app().click(app().find("[data-voice-hands-free]"));
    await app().act(async () => app().host.voiceEvent({ type: "speechStart" }));
    await app().act(async () =>
      app().host.voiceEvent({ type: "final", text: "spoken" }),
    );
    await app().settle();
    await app().act(async () => deliver());
    await app().settle();
    const messages = app().host.sent.map((sent) => sent.params.message);
    expect(messages).toEqual([
      "typed\nUploaded file: notes.txt\n/w/.twr-uploads/u/notes.txt",
      "spoken",
    ]);
    expect(chips()).toHaveLength(0);
  });
});

/* Usage limits (docs/desktop.md › Usage limits): `/usage` opens them above the message box,
 * the header's gauge in a popover; both read `usage.get` when they open and when the hub's
 * live view says an account's usage changed. */
describe("usage limits", () => {
  let reads = 0;
  const usage = {
    now: "2026-11-20T12:00:00Z",
    runtimes: [
      {
        runtime: "claude-code",
        account: null,
        label: "Claude",
        plan: "max",
        age_secs: 5,
        stale: false,
        windows: [
          {
            window: "five_hour",
            label: "5h",
            used_pct: 80,
            remaining_pct: 20,
            minutes: 300,
          },
        ],
      },
    ],
  };
  beforeEach(async () => {
    vi.resetModules();
    reads = 0;
    mounted = await mount({
      rpc: (method) => {
        if (method === "usage.get") {
          reads++;
          return Promise.resolve(usage);
        }
        return undefined;
      },
    });
  });
  afterEach(async () => {
    await mounted.unmount();
  });

  it("opens with /usage above the message box, and closes", async () => {
    expect(reads).toBe(0);
    await app().type("/usage");
    await app().press("Enter");
    const panel = app().find(".usage-panel");
    expect(panel.textContent).toContain("Usage limits");
    expect(panel.textContent).toContain("Claude");
    expect(panel.textContent).toContain("20% left");
    expect(panel.querySelector(".usage-row")!.getAttribute("data-level")).toBe(
      "warn",
    );
    expect(app().box().value).toBe("");
    expect(app().host.sent).toEqual([]);
    expect(reads).toBe(1);
    await app().click(
      app().find('.usage-panel button[aria-label="Close usage limits"]'),
    );
    expect(app().all(".usage-panel")).toHaveLength(0);
  });

  it("closes with Esc from the empty message box, staying in the thread's conversation", async () => {
    await app().talkTo("builder");
    await app().type("/usage");
    await app().press("Enter");
    expect(app().all(".usage-panel")).toHaveLength(1);
    await app().press("Escape");
    expect(app().all(".usage-panel")).toHaveLength(0);
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
  });

  it("closes with Esc while the message box holds a draft, keeping the draft", async () => {
    await app().type("/usage");
    await app().press("Enter");
    await app().type("half a thought");
    await app().press("Escape");
    expect(app().all(".usage-panel")).toHaveLength(0);
    expect(app().box().value).toBe("half a thought");
  });

  it("reads again when the hub's live view says usage changed", async () => {
    await app().type("/usage");
    await app().press("Enter");
    expect(reads).toBe(1);
    await app().act(async () => {
      app().host.emit({
        type: "state",
        state: {
          threads: views.default,
          attention: [],
          decisions: [],
          decisions_seen: 0,
          head: 2,
          usage: [{ runtime: "claude-code", windows: [] }],
        },
      } as DesktopEvent);
    });
    await app().wait(60);
    expect(reads).toBe(2);
  });

  it("opens from the header's gauge as a popover, with the way to the Usage page", async () => {
    const gauge = app().find(
      '.conversation-actions button[aria-label="Usage limits"]',
    );
    expect(gauge.getAttribute("aria-expanded")).toBe("false");
    await app().click(gauge);
    expect(gauge.getAttribute("aria-expanded")).toBe("true");
    const popover = app().find(".usage-popover");
    expect(popover.getAttribute("role")).toBe("dialog");
    expect(popover.textContent).toContain("20% left");
    await app().click(app().find(".usage-popover .usage-page-link"));
    expect(app().all(".usage-popover")).toHaveLength(0);
    expect(app().find(".manager h1").textContent).toBe("Usage");
    expect(app().find(".usage-cards").textContent).toContain("20% left");
  });

  it("closes the popover with Esc from the gauge too", async () => {
    const gauge = app().find(
      '.conversation-actions button[aria-label="Usage limits"]',
    );
    await app().click(gauge);
    expect(app().all(".usage-popover")).toHaveLength(1);
    // Shift-Tab from the popover lands on the gauge, which the focus check leaves alone.
    await app().act(async () => void press(gauge, "Escape"));
    expect(app().all(".usage-popover")).toHaveLength(0);
    expect(gauge.getAttribute("aria-expanded")).toBe("false");
  });

  it("closes the popover when the focus moves outside it", async () => {
    const gauge = app().find(
      '.conversation-actions button[aria-label="Usage limits"]',
    );
    await app().click(gauge);
    expect(app().all(".usage-popover")).toHaveLength(1);
    // Tab past its last control, to the message box: closed, Esc would no longer reach it.
    await app().act(async () => {
      app().box().focus();
      app()
        .box()
        .dispatchEvent(new FakeEvent("focusin", { bubbles: true }));
    });
    expect(app().all(".usage-popover")).toHaveLength(0);
    expect(gauge.getAttribute("aria-expanded")).toBe("false");
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

  it("closes the choice when the focus moves outside it", async () => {
    await app().talkTo("builder");
    await app().click(app().find(".send-caret"));
    expect(app().all(".send-loop-panel")).toHaveLength(1);
    await app().act(async () => {
      app().box().focus();
      app()
        .box()
        .dispatchEvent(new FakeEvent("focusin", { bubbles: true }));
    });
    expect(app().all(".send-loop-panel")).toHaveLength(0);
    expect(asked).toEqual([]);
  });

  it("closes the choice with Esc from its button, staying in the conversation", async () => {
    await app().talkTo("builder");
    const caret = app().find(".send-caret");
    await app().click(caret);
    expect(app().all(".send-loop-panel")).toHaveLength(1);
    // Shift-Tab from the panel lands on its button, the panel still open.
    await app().act(async () => void press(caret, "Escape"));
    expect(app().all(".send-loop-panel")).toHaveLength(0);
    expect(app().box().getAttribute("aria-label")).toBe(
      "Message builder directly",
    );
  });

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

/* The palette's Switch orchestrator form: `[orchestrator] effort` applies to the first
 * orchestrator only, so with one running, an unset effort is the runtime's own and says so. */
describe("Switch orchestrator's effort", () => {
  beforeEach(async () => {
    vi.resetModules();
    mounted = await mount({
      rpc: (method, params) => {
        if (method === "orchestrator.show")
          return Promise.resolve({
            thread: "orch",
            models: { fake: { models: [{ id: "m1" }], efforts: ["low"] } },
          });
        if (method === "desktop.parse_slash" && params.line.trim() === "/orch")
          return Promise.resolve({ kind: "OrchPick" });
        return undefined;
      },
    });
    await app().wait(50);
  });
  afterEach(async () => {
    await mounted.unmount();
  });

  it("offers Runtime default, not As configured, while an orchestrator runs", async () => {
    await app().type("/orch");
    await app().press("Enter");
    const form = app().find("[role=dialog]", "Switch orchestrator");
    const options = form
      .querySelectorAll("option")
      .map((o: FakeElement) => o.textContent);
    expect(options).toContain("low");
    expect(options.join("|")).not.toContain("As configured");
    expect(options).toContain("Runtime default");
  });
});

/* `/access [limited|full]` (docs/orchestrator.md › Full access): the shared slash table's
 * command opens the palette's Orchestrator access form on the level named, and Full still
 * asks before anything reaches the hub. */
describe("/access in the message box", () => {
  const asked: { method: string; params: Json }[] = [];
  beforeEach(async () => {
    vi.resetModules();
    asked.length = 0;
    mounted = await mount({
      rpc: (method, params) => {
        if (method === "hub.info")
          return Promise.resolve({
            workspace: "default",
            orchestrator: { access: "limited" },
          });
        if (method === "orchestrator.access") {
          asked.push({ method, params });
          return Promise.resolve({
            access: params.access,
            previous: "limited",
            persisted: true,
            restart: "now",
          });
        }
        if (method === "desktop.parse_slash") {
          const words = String(params.line).trim().split(/\s+/);
          if (words[0] === "/access")
            return Promise.resolve({ kind: "Access", args: words[1] ?? null });
        }
        return undefined;
      },
    });
    // The app's next hub.info poll reads the level.
    await app().wait(50);
  });
  afterEach(async () => {
    await mounted.unmount();
  });
  /** Submits the open Orchestrator access form. */
  const submit = async () => {
    await app().act(async () => {
      app()
        .find("[role=dialog] form", "Orchestrator access")
        .dispatchEvent(
          new FakeEvent("submit", { bubbles: true, cancelable: true }),
        );
    });
    await app().settle();
  };

  it("opens the Orchestrator access form on the level named, without calling the hub", async () => {
    await app().type("/access full");
    await app().press("Enter");
    app().find("[role=dialog]", "Orchestrator access");
    expect(asked).toEqual([]);
    expect(app().host.sent).toEqual([]);
    // Submitting it asks first, as Full does: the form holds the level named.
    await submit();
    expect(
      app().find("[role=dialog]", "Give the orchestrator full access?"),
    ).toBeTruthy();
    expect(asked).toEqual([]);
  });

  it("runs /access limited from the form without asking", async () => {
    await app().type("/access limited");
    await app().press("Enter");
    await submit();
    expect(asked).toEqual([
      { method: "orchestrator.access", params: { access: "limited" } },
    ]);
  });
});

describe("messages put back in the box stay messages", () => {
  afterEach(async () => {
    await mounted.unmount();
  });
  const start = async (options?: Parameters<typeof mount>[0]) => {
    vi.resetModules();
    mounted = await mount(options);
  };
  const sentMessages = () =>
    app().host.sent.map((s) => s.params.message ?? s.params.prompt);

  it("sends a leading ! with \\!, and a leading \\! with one more backslash", async () => {
    await start();
    await app().type("\\!ls is what I ran");
    await app().press("Enter");
    await app().type("\\\\!x");
    await app().press("Enter");
    await app().type("!!!wow");
    await app().press("Enter");
    expect(sentMessages()).toEqual(["!ls is what I ran", "\\!x", "!!!wow"]);
  });

  it("takes back a queued / or ! message escaped, and sends it as a message again", async () => {
    await start({
      rpc: (method) =>
        method === "shell.run" ? Promise.resolve({ job: "j1" }) : undefined,
    });
    app().host.queues.w1 = [
      { ref: 1, prompt: "/usage", mine: true },
      { ref: 2, prompt: "!ls", mine: true },
    ];
    await app().talkTo("builder");
    await app().click(app().find(".queued-message button", "Take back"));
    expect(app().box().value).toBe("//usage");
    await app().press("Enter");
    await app().click(app().find(".queued-message button", "Take back"));
    expect(app().box().value).toBe("\\!ls");
    await app().press("Enter");
    expect(app().all(".usage-panel")).toHaveLength(0);
    expect(app().host.sent.map((s) => s.method)).toEqual([
      "thread.send",
      "thread.send",
    ]);
    expect(sentMessages()).toEqual(["/usage", "!ls"]);
  });

  it("keeps a taken-back message when the box holds a command, sending both as text", async () => {
    await start();
    app().host.queues.w1 = [{ ref: 1, prompt: "taken back", mine: true }];
    await app().talkTo("builder");
    await app().type("/usage");
    await app().click(app().find(".queued-message button", "Take back"));
    expect(app().box().value).toBe("//usage\n\ntaken back");
    await app().press("Enter");
    expect(app().all(".usage-panel")).toHaveLength(0);
    expect(sentMessages()).toEqual(["/usage\n\ntaken back"]);
  });

  it("opens the orchestrator's main conversation for /open orchestrator, its picker changing it in place", async () => {
    const catalog = { fake: { models: [{ id: "m1" }], efforts: ["low"] } };
    const calls: { method: string; params: Json }[] = [];
    await start({
      rpc: (method, params) => {
        if (method === "desktop.parse_slash")
          return Promise.resolve({ kind: "Open", args: "orchestrator" });
        if (method === "orchestrator.show")
          return Promise.resolve({ models: catalog, thread: "orch" });
        if (method === "orchestrator.use" || method === "thread.set_model") {
          calls.push({ method, params });
          return Promise.resolve({ thread: "orch" });
        }
        return undefined;
      },
    });
    await app().talkTo("builder");
    await app().type("/open orchestrator");
    await app().press("Enter");
    // Its main conversation, not a thread's: no way back to it, sent with `say`.
    expect(app().all(".destination-symbol.worker").length).toBe(0);
    await app().type("hello");
    await app().press("Enter");
    expect(app().host.sent.map((s) => s.method)).toEqual(["say"]);
    // Its picker changes the orchestrator's own thread, in place, with nothing to confirm.
    await app().click(app().find(".model-picker-trigger"));
    const search = app().find(".model-picker-search");
    await app().act(async () => void press(search, "Enter"));
    await app().click(
      app()
        .all(".model-picker-efforts [role=radio]")
        .find((n) => n.textContent.includes("Low"))! as FakeElement,
    );
    expect(
      app()
        .all("button")
        .some((b) => b.textContent === "Switch without summary"),
    ).toBe(false);
    expect(calls).toEqual([
      { method: "thread.set_model", params: { thread: "orch", effort: "low" } },
    ]);
  });

  it("puts back what a switch dropped escaped, and says the new orchestrator can't start", async () => {
    const catalog = {
      fake: { models: [{ id: "m1" }], efforts: ["low"] },
      codex: { models: [{ id: "gpt-5" }], efforts: ["low"] },
    };
    const uses: Json[] = [];
    await start({
      rpc: (method, params) => {
        if (method === "orchestrator.show")
          return Promise.resolve({
            models: catalog,
            thread: "orch",
            account: "default",
            installed: { fake: true, codex: true },
          });
        if (method === "orchestrator.use") {
          uses.push(params);
          return Promise.resolve({
            thread: "orch",
            previous: {
              dropped_prompts: 2,
              dropped_messages: ["/usage", "!ls"],
            },
            warning:
              "fake is not on PATH: the new orchestrator cannot start until it is there (install it)",
          });
        }
        return undefined;
      },
    });
    // A switch to another runtime, confirmed without the summary.
    await app().click(app().find(".model-picker-trigger"));
    await app().click(
      app()
        .all(".model-picker-rail [role=option]")
        .find((n) => n.textContent.includes("codex"))! as FakeElement,
    );
    const search = app().find(".model-picker-search");
    await app().act(async () => void press(search, "Enter"));
    await app().click(
      app()
        .all(".model-picker-efforts [role=radio]")
        .find((n) => n.textContent.includes("Low"))! as FakeElement,
    );
    await app().click(app().find("button", "Switch without summary"));
    expect(uses).toEqual([{ runtime: "codex", effort: "low", summary: false }]);
    expect(app().find(".notice-toast").textContent).toContain(
      "fake is not on PATH: the new orchestrator cannot start until it is there (install it).",
    );
    await app().click(app().find("button", "Put back"));
    // Escaped only where it starts the box: after other text a / or ! is a message's.
    expect(app().box().value).toBe("//usage\n\n!ls");
    await app().press("Enter");
    expect(app().all(".usage-panel")).toHaveLength(0);
    expect(sentMessages()).toEqual(["/usage\n\n!ls"]);
  });
});
