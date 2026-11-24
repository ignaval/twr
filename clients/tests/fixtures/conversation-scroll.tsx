// Browser-only fixture for conversation-scroll-qa.mjs: real App and virtualizer,
// a disposable host whose older transcript requests resolve only on demand.
import { createRoot } from "react-dom/client";
import App from "../../shared/App";
import type {
  Attention,
  DesktopAPI,
  DesktopEvent,
  HubEvent,
  Json,
  Thread,
} from "../../shared/types";
import "../../shared/style.css";
import "../../mobile/src/mobile.css";

const listeners = new Set<(event: DesktopEvent) => void>();
let currentThreads: Thread[] = [];
const recorded: HubEvent[] = [];
const historyReads: HubEvent[][] = [];
const emit = (event: DesktopEvent) => {
  if (event.type === "state" && event.state)
    currentThreads = event.state.threads;
  if (event.type === "thread" && event.event) recorded.push(event.event);
  listeners.forEach((read) => read(event));
};
const pending = new Map<string, (page: Json) => void>();
let subscription = "";
let sequence = 500;
const withUser = new URLSearchParams(location.search).has("with-user");
const combined = new URLSearchParams(location.search).has("combined");
const short = new URLSearchParams(location.search).has("short");
const sparse = new URLSearchParams(location.search).has("sparse");
const fastHistory = new URLSearchParams(location.search).has("fast-history");
const historyRequests: string[] = [];
// Replay streamed final-only history only in the identity-cutoff reload scenario.
const replayHistory = new URLSearchParams(location.search).has(
  "replay-history",
);
const event = (
  thread: string,
  turn: number,
  seq: number,
  kind: string,
  data: Json,
): HubEvent => ({
  id: `${thread}:${seq}`,
  thread,
  turn,
  seq,
  kind,
  data,
  ts: "2026-11-17T12:00:00Z",
});
function history(thread: string, turn: number) {
  return [
    event(thread, turn, turn * 100, "turn.started", {
      from:
        withUser && (!sparse || turn === 4 || turn === 1)
          ? "user"
          : "orchestrator",
      prompt: `Task ${thread}:${turn}`,
    }),
    ...Array.from({ length: short ? 1 : 60 }, (_, index) =>
      event(thread, turn, turn * 100 + index + 1, "agent.text", {
        text: short
          ? "Short reply."
          : combined && index === 59
            ? 'Summary before boxes.\n\n<thread name="builder">\n**Ready** with `code` and [shared reference][guide].\n</thread>\n\n<thread name="reviewer">\n- Checked Markdown\n- Checked navigation\n</thread>\n\n<thread name="missing">\nUnknown worker update.\n</thread>\n\n[guide]: https://example.com/guide'
            : `Message ${thread}:${turn}:${index}\n\n${"Variable row height. ".repeat(5 + (index % 9) * 13)}`,
      }),
    ),
  ];
}

declare global {
  interface Window {
    conversationScrollQA: {
      pending: () => string[];
      historyRequests: () => string[];
      release: (thread: string, turn: number) => void;
      releaseNext: () => void;
      append: () => void;
      threads: () => Thread[];
      setThreads: (threads: Thread[], attention?: Attention[]) => void;
      reply: (text: string, ts: string) => void;
      stream: (kind: string, data: Json, ts: string) => void;
      historyReads: () => HubEvent[][];
      appendUser: (prompt?: string) => void;
    };
  }
}
window.conversationScrollQA = {
  historyReads: () => historyReads,
  stream: (kind, data, ts) =>
    emit({
      type: "thread",
      subscription,
      event: { ...event("orch", 5, sequence++, kind, data), ts },
    }),
  threads: () => currentThreads,
  setThreads: (threads, attention = []) =>
    emit({
      type: "state",
      state: {
        threads,
        attention,
        decisions: [],
        decisions_seen: 0,
        head: 1,
      },
    }),
  reply: (text, ts) => {
    emit({
      type: "thread",
      subscription,
      event: {
        ...event("orch", 6, sequence++, "turn.started", {
          from: "user",
          prompt: "New worker task",
        }),
        ts,
      },
    });
    emit({
      type: "thread",
      subscription,
      event: {
        ...event("orch", 6, sequence++, "agent.text", { text, final: true }),
        ts,
      },
    });
  },
  pending: () => [...pending.keys()],
  historyRequests: () => [...historyRequests],
  releaseNext: () => {
    const resolve = pending.get("orch:2:next");
    if (!resolve) throw new Error("No pending next page");
    pending.delete("orch:2:next");
    resolve({ events: [], turn: 2, more: false });
  },
  release: (thread, turn) => {
    const key = `${thread}:${turn}`;
    const resolve = pending.get(key);
    if (!resolve) throw new Error(`No pending transcript request: ${key}`);
    pending.delete(key);
    resolve({
      events: history(thread, turn),
      turn,
      more: sparse && turn === 2,
      next: turn * 100 + 60,
    });
  },
  append: () =>
    emit({
      type: "thread",
      subscription,
      event: event("orch", 5, sequence++, "agent.text", {
        text: "Live reply below the viewport. ".repeat(200),
      }),
    }),
  appendUser: (prompt = "Live user request") =>
    emit({
      type: "thread",
      subscription,
      event: event("orch", 5, sequence++, "turn.started", {
        from: "user",
        prompt,
      }),
    }),
};
window.twr = {
  bootstrap: async () => ({
    workspace: "default",
    hostId: "scroll-qa",
    platform: "linux",
    home: "/tmp/scroll-qa",
    version: "0.0.0",
    theme: null,
    terminals: [],
  }),
  connect: async () => {
    setTimeout(() => {
      emit({ type: "connection", connected: true });
      emit({
        type: "state",
        state: {
          threads: [
            {
              id: "orch",
              name: "orchestrator",
              created_at: "2026-11-16T12:00:00Z",
              is_orchestrator: true,
              runtime: "fake",
              box_name: "local",
              mode: "default",
              status: "idle",
              accent: "#3366ff",
            },
            ...(combined
              ? [
                  {
                    id: "builder",
                    name: "builder",
                    created_at: "2026-11-16T12:00:00Z",
                    is_orchestrator: false,
                    runtime: "fake",
                    box_name: "local",
                    mode: "default",
                    status: "idle",
                    accent: "#0087af",
                  },
                  {
                    id: "reviewer",
                    name: "reviewer",
                    created_at: "2026-11-16T12:00:00Z",
                    is_orchestrator: false,
                    runtime: "fake",
                    box_name: "local",
                    mode: "default",
                    status: "idle",
                    accent: "#ff00ff",
                  },
                ]
              : []),
          ],
          attention: [],
          decisions: [],
          decisions_seen: 0,
          head: 1,
        },
      });
    }, 0);
    return {};
  },
  rpc: async (method: string, params: Json = {}) => {
    if (method === "desktop.subscribe") subscription = params.subscription;
    if (method === "thread.transcript") {
      const logged = recorded.filter((event) => event.thread === params.thread);
      if (
        replayHistory &&
        logged.some((event) => event.kind === "turn.started") &&
        !params.turn
      ) {
        const turn = logged
          .filter((event) => event.kind === "turn.started")
          .at(-1)!.turn;
        // Match read_transcript_events: keep semantic rows, filter deltas before
        // paging. History reloads must not reuse the live first-delta timestamp.
        const events = logged.filter(
          (event) =>
            event.turn === turn &&
            event.seq > (params.since_seq || 0) &&
            (event.kind !== "agent.text" || event.data.final === true),
        );
        historyReads.push(events);
        return { events, turn, next: logged.at(-1)!.seq, more: false };
      }
      if (params.turn) {
        historyRequests.push(`${params.thread}:${params.turn}`);
        if (fastHistory)
          return {
            events: history(params.thread, params.turn),
            turn: params.turn,
            more: false,
          };
      }
      // An older automatic turn spanning two pages. The first page must not
      // become visible before the full turn has arrived.
      if (sparse && params.turn === 2 && params.since_seq)
        return new Promise<Json>((resolve) => {
          pending.set(`${params.thread}:2:next`, resolve);
        });
      if (params.since_seq) return { events: [], more: false };
      if (params.turn)
        return new Promise<Json>((resolve) => {
          pending.set(`${params.thread}:${params.turn}`, resolve);
        });
      return {
        events: short
          ? [1, 2, 3, 4].flatMap((turn) => history(params.thread, turn))
          : history(params.thread, 4),
        turn: short ? 1 : 4,
        next: 460,
        more: false,
      };
    }
    if (["thread.queue", "desktop.slash_commands"].includes(method)) return [];
    return {};
  },
  cli: async () => ({ stdout: '["default"]', stderr: "", code: 0 }),
  catalog: async () => [],
  configRead: async () => ({ content: "", hash: "", path: "", settings: {} }),
  onEvent: (read: (event: DesktopEvent) => void) => {
    listeners.add(read);
    return () => listeners.delete(read);
  },
  secureRead: async () => null,
  secureWrite: async () => {},
  secureDelete: async () => {},
} as unknown as DesktopAPI;
createRoot(document.getElementById("root")!).render(<App />);
