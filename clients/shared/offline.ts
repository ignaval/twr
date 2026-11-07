import type { DesktopAPI, HubEvent, Json, ViewState } from "./types";
import { requestId } from "./remote-crypto";

export interface DurableMessage {
  id: string;
  workspace: string;
  method: "say" | "thread.send";
  params: Json;
  createdAt: number;
}
export interface OutboxMessage extends DurableMessage {
  draft?: string;
  state: "queued" | "sending" | "review";
  error?: string;
}
export function appendDraft(current: string | undefined, text: string) {
  if (!current) return text;
  return current === text || current.endsWith("\n\n" + text)
    ? current
    : current + "\n\n" + text;
}
export function messageDraft(message: OutboxMessage) {
  if (typeof message.draft === "string") return message.draft;
  const text = String(message.params.prompt || message.params.message || "");
  return text.startsWith("/") ? "/" + text : text;
}
export interface CachedWorkspace {
  savedAt: number;
  view: ViewState;
  transcripts: Record<string, HubEvent[]>;
  transcriptTimes?: Record<string, number>;
}
export interface OfflineRecord {
  version: 1;
  drafts: Record<string, string>;
  workspaces: Record<string, CachedWorkspace>;
  outbox: OutboxMessage[];
}
import { RECEIPT_WINDOW } from "./offline-constants";
export { RECEIPT_WINDOW } from "./offline-constants";
const HISTORY_WINDOW = 7 * 24 * 60 * 60 * 1000;
const bytes = (value: unknown) =>
  new TextEncoder().encode(JSON.stringify(value)).byteLength;
export function messageOnlyParams(
  method: DurableMessage["method"],
  params: Json,
): Json {
  if (method !== "say") return params;
  const { seen_gseq: _, focused: __, ...message } = params;
  return { ...message, to: "orchestrator" };
}
const forgottenHosts = new Set<string>();
const activeJournals = new Map<string, Set<WeakRef<OfflineJournal>>>();
export const isOfflineHostForgotten = (id: string) => forgottenHosts.has(id);
export async function forgetOfflineHost(
  hostId: string,
  store: Pick<DesktopAPI, "secureDelete">,
) {
  forgottenHosts.add(hostId);
  const entries = activeJournals.get(hostId);
  await Promise.all(
    [...(entries || [])].map(async (ref) => {
      const journal = ref.deref();
      if (journal) await journal.retire();
    }),
  );
  activeJournals.delete(hostId);
  journals.delete(hostId);
  for (const key of [
    `offline:${hostId}`,
    `offline-pending:${hostId}`,
    `bootstrap:${hostId}`,
  ])
    await store.secureDelete?.(key);
}
export function tombstoneOfflineHost(hostId: string) {
  forgottenHosts.add(hostId);
}
export function activateOfflineHost(hostId: string) {
  forgottenHosts.delete(hostId);
}
function blank(): OfflineRecord {
  return { version: 1, drafts: {}, workspaces: {}, outbox: [] };
}
export function parseOffline(
  value: string | null,
  now = Date.now(),
): OfflineRecord {
  if (!value) return blank();
  const record = JSON.parse(value);
  if (
    record.version !== 1 ||
    !record.drafts ||
    !record.workspaces ||
    !Array.isArray(record.outbox)
  )
    throw new Error("Unsupported offline history format");
  const outbox: OutboxMessage[] = record.outbox
    .filter(
      (item: OutboxMessage) =>
        typeof item.id === "string" &&
        typeof item.workspace === "string" &&
        ["say", "thread.send"].includes(item.method) &&
        Number.isFinite(item.createdAt),
    )
    .map((item: OutboxMessage) => ({
      ...item,
      params: messageOnlyParams(item.method, item.params),
      state:
        now - item.createdAt >= RECEIPT_WINDOW || item.state === "review"
          ? "review"
          : "queued",
      ...(now - item.createdAt >= RECEIPT_WINDOW
        ? {
            error:
              "This message is older than the safe retry window. Check the conversation before sending it again.",
          }
        : {}),
    }));
  return {
    version: 1,
    drafts: Object.fromEntries(
      Object.entries(record.drafts).filter(
        (entry): entry is [string, string] => typeof entry[1] === "string",
      ),
    ),
    workspaces: Object.fromEntries(
      Object.entries(record.workspaces as Record<string, CachedWorkspace>)
        .filter(
          ([, value]) =>
            Number.isFinite(value.savedAt) &&
            now - value.savedAt < HISTORY_WINDOW &&
            value.view &&
            Array.isArray(value.view.threads) &&
            value.transcripts,
        )
        .map(([key, value]) => [
          key,
          {
            ...value,
            transcripts: Object.fromEntries(
              Object.entries(value.transcripts).filter(
                ([id]) =>
                  now - (value.transcriptTimes?.[id] || value.savedAt) <
                  HISTORY_WINDOW,
              ),
            ),
          },
        ]),
    ),
    outbox: outbox.slice(0, 100),
  };
}
/** One serialized encrypted record per host prevents draft/cache/outbox write races. */
export class OfflineJournal {
  historyError = "";
  private record = blank();
  private loaded?: Promise<void>;
  private tail: Promise<unknown> = Promise.resolve();
  private listeners = new Set<() => void>();
  private active = true;
  private generation = 0;
  constructor(
    private host: DesktopAPI,
    readonly hostId: string,
  ) {
    const entries =
      activeJournals.get(hostId) || new Set<WeakRef<OfflineJournal>>();
    entries.add(new WeakRef(this));
    activeJournals.set(hostId, entries);
  }
  rebind(host: DesktopAPI) {
    this.host = host;
  }
  get key() {
    return `offline:${this.hostId}`;
  }
  get pendingKey() {
    return `offline-pending:${this.hostId}`;
  }
  get snapshot() {
    return this.record;
  }
  get epoch() {
    return this.generation;
  }
  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };
  private changed() {
    this.listeners.forEach((fn) => fn());
  }
  load() {
    if (!this.loaded)
      this.loaded = (async () => {
        if (!this.host.secureRead || !this.host.secureWrite)
          throw new Error(
            "Encrypted offline storage is unavailable on this device",
          );
        // History is disposable; a damaged cache must not hide readable drafts.
        const [historyRead, pendingRead] = await Promise.allSettled([
          this.host.secureRead(this.key),
          this.host.secureRead(this.pendingKey),
        ]);
        if (pendingRead.status === "rejected") throw pendingRead.reason;
        const savedPending = pendingRead.value;
        let historyRaw: string | null = null;
        let history = blank();
        try {
          if (historyRead.status === "rejected") throw historyRead.reason;
          historyRaw = historyRead.value;
          history = parseOffline(historyRaw);
          this.historyError = "";
        } catch {
          this.historyError =
            "Saved history could not be read. Drafts and outbox are still available. Retry or clear saved history in Settings.";
          historyRaw = null;
        }
        const pending =
          savedPending === null ? history : parseOffline(savedPending);
        this.record = {
          ...history,
          drafts: pending.drafts,
          outbox: pending.outbox,
        };
        if (!this.active || forgottenHosts.has(this.hostId))
          throw new Error("This hub was forgotten");
        const original = historyRaw ? JSON.parse(historyRaw) : undefined;
        const migrate =
          savedPending === null &&
          (Object.keys(history.drafts).length || history.outbox.length);
        if (migrate)
          await this.host.secureWrite(
            this.pendingKey,
            JSON.stringify({ ...this.record, workspaces: {} }),
          );
        if (!this.active || forgottenHosts.has(this.hostId))
          throw new Error("This hub was forgotten");
        if (
          original &&
          (migrate ||
            JSON.stringify(original.workspaces) !==
              JSON.stringify(history.workspaces))
        )
          await this.host.secureWrite(
            this.key,
            JSON.stringify({ ...history, drafts: {}, outbox: [] }),
          );
        this.changed();
      })().catch((error) => {
        this.loaded = undefined;
        throw error;
      });
    return this.loaded;
  }
  private update(
    change: (record: OfflineRecord) => OfflineRecord,
    lane: "pending" | "history" = "pending",
  ) {
    const generation = this.generation;
    const job = this.tail
      .catch(() => {})
      .then(async () => {
        await this.load();
        if (
          !this.active ||
          forgottenHosts.has(this.hostId) ||
          generation !== this.generation
        )
          throw new Error(
            "Offline history was cleared; reopen this hub to resume saving",
          );
        let next = change(this.record);
        // Preserve drafts and queued messages first. Evict oldest history until bounded.
        while (
          bytes(next) > 4 * 1024 * 1024 &&
          Object.keys(next.workspaces).length
        ) {
          const oldest = Object.entries(next.workspaces).sort(
            (a, b) => a[1].savedAt - b[1].savedAt,
          )[0][0];
          const { [oldest]: _, ...workspaces } = next.workspaces;
          next = { ...next, workspaces };
        }
        if (bytes(next) > 6 * 1024 * 1024)
          throw new Error(
            "Offline drafts and messages are full. Remove an outbox item before adding more.",
          );
        const payload =
          lane === "history"
            ? { ...next, drafts: {}, outbox: [] }
            : { ...next, workspaces: {} };
        await this.host.secureWrite!(
          lane === "history" ? this.key : this.pendingKey,
          JSON.stringify(payload),
        );
        this.record = next;
        if (lane === "history") this.historyError = "";
        this.changed();
      });
    this.tail = job;
    return job;
  }
  drafts(drafts: Record<string, string>, epoch = this.generation) {
    if (epoch !== this.generation)
      return Promise.reject(
        new Error("Offline storage changed before saving this draft"),
      );
    return this.update((record) => ({ ...record, drafts }));
  }
  cache(
    workspace: string,
    view: ViewState,
    thread?: string,
    events?: HubEvent[],
  ) {
    if (this.historyError) return Promise.resolve();
    return this.update((record) => {
      const previous = record.workspaces[workspace];
      const transcriptTimes = { ...previous?.transcriptTimes };
      const transcripts = Object.fromEntries(
        Object.entries(previous?.transcripts || {}).filter(
          ([id]) =>
            Date.now() - (transcriptTimes[id] || previous?.savedAt || 0) <
            HISTORY_WINDOW,
        ),
      );
      if (thread && events) {
        transcriptTimes[thread] = Date.now();
        transcripts[thread] = events
          .slice(-500)
          .filter((event) => bytes(event) <= 128 * 1024);
        for (const id of Object.keys(transcripts)
          .sort((a, b) => (transcriptTimes[a] || 0) - (transcriptTimes[b] || 0))
          .slice(0, Math.max(0, Object.keys(transcripts).length - 8)))
          if (id !== thread) delete transcripts[id];
      }
      // Never cache credentials/configuration or full tool payloads above the bound.
      const compact: ViewState = {
        ...view,
        threads: view.threads
          .filter((item) => bytes(item) <= 128 * 1024)
          .slice(0, 200),
        attention: view.attention.filter((item) => bytes(item) <= 128 * 1024),
        decisions: view.decisions
          .filter((item) => bytes(item) <= 64 * 1024)
          .slice(-40),
      };
      return {
        ...record,
        workspaces: {
          ...record.workspaces,
          [workspace]: {
            savedAt: Date.now(),
            view: compact,
            transcripts,
            transcriptTimes,
          },
        },
      };
    }, "history");
  }
  async enqueue(
    workspace: string,
    method: DurableMessage["method"],
    params: Json,
    draft?: string,
  ) {
    const message: OutboxMessage = {
      id: requestId(),
      workspace,
      method,
      params: messageOnlyParams(method, params),
      createdAt: Date.now(),
      state: "queued",
      ...(draft === undefined ? {} : { draft }),
    };
    await this.update((record) => {
      if (record.outbox.length >= 100)
        throw new Error("Outbox is full (100 messages)");
      const key =
        workspace + ":" + (method === "say" ? "orchestrator" : params.thread);
      return {
        ...record,
        drafts: { ...record.drafts, [key]: "" },
        outbox: [...record.outbox, message],
      };
    });
    return message;
  }
  private removed = new Set<string>();
  remove(id: string) {
    this.removed.add(id);
    if (this.removed.size > 1024)
      this.removed.delete(this.removed.values().next().value!);
    return this.update((record) => ({
      ...record,
      outbox: record.outbox.filter((item) => item.id !== id),
    }));
  }
  clearHistory() {
    return this.update((record) => ({ ...record, workspaces: {} }), "history");
  }
  async retryHistory() {
    const generation = this.generation;
    await this.load();
    const history = parseOffline(await this.host.secureRead!(this.key));
    if (!this.active || generation !== this.generation)
      throw new Error("Offline storage changed while reading history");
    await this.update(
      (record) => ({ ...record, workspaces: history.workspaces }),
      "history",
    );
  }
  async moveToDraft(id: string) {
    await this.update((record) => {
      const message = record.outbox.find((item) => item.id === id);
      if (!message) throw new Error("This saved message was removed");
      if (message.state === "sending" || this.inFlight.has(id))
        throw new Error(
          "Wait for this message's delivery result before editing it",
        );
      this.removed.add(id);
      const key =
        message.workspace +
        ":" +
        (message.method === "say" ? "orchestrator" : message.params.thread);
      return {
        ...record,
        outbox: record.outbox.filter((item) => item.id !== id),
        drafts: {
          ...record.drafts,
          [key]: appendDraft(record.drafts[key], messageDraft(message)),
        },
      };
    });
  }
  async reset() {
    ++this.generation;
    this.active = false;
    await Promise.allSettled([this.tail, this.loaded]);
    this.deliveryTail = Promise.resolve();
    await this.host.secureDelete?.(this.key);
    await this.host.secureDelete?.(this.pendingKey);
    this.active = true;
    forgottenHosts.delete(this.hostId);
    this.record = blank();
    this.loaded = undefined;
    await this.load();
    this.changed();
  }
  private deliveryTail: Promise<unknown> = Promise.resolve();
  private inFlight = new Map<string, Promise<Json>>();
  async deliver(requested: OutboxMessage, retryReview = false) {
    await this.load();
    const index = this.record.outbox.findIndex(
      (item) => item.id === requested.id,
    );
    const restored: Json[] = [];
    for (const prior of this.record.outbox
      .slice(0, Math.max(0, index))
      .filter(
        (item) =>
          item.workspace === requested.workspace && item.state !== "review",
      )) {
      try {
        const result = await this.deliverOne(prior);
        if (result.restored_draft) restored.push(result.restored_draft);
      } catch (error) {
        const current = this.record.outbox.find((item) => item.id === prior.id);
        if (current && current.state !== "review") throw error;
      }
    }
    const result = await this.deliverOne(requested, retryReview);
    return restored.length ? { ...result, restored_drafts: restored } : result;
  }
  private deliverOne(requested: OutboxMessage, retryReview = false) {
    const generation = this.generation;
    let message = requested;
    const existing = this.inFlight.get(message.id);
    if (existing) return existing;
    const run = this.deliveryTail
      .catch(() => {})
      .then(async () => {
        await this.load();
        // A queued snapshot is not authority to send an item removed while waiting.
        await this.tail.catch(() => {});
        const stored = this.record.outbox.find(
          (item) => item.id === requested.id,
        );
        if (
          !stored ||
          forgottenHosts.has(this.hostId) ||
          !this.active ||
          generation !== this.generation
        )
          throw new Error("This saved message was removed");
        message = stored;
        if (!this.host.sendMessage)
          throw new Error(
            "This app cannot safely retry saved messages. Update it first.",
          );
        if (
          (message.state === "review" && !retryReview) ||
          Date.now() - message.createdAt >= RECEIPT_WINDOW
        ) {
          await this.update((record) => ({
            ...record,
            outbox: record.outbox.map((item) =>
              item.id === message.id
                ? {
                    ...item,
                    state: "review",
                    error:
                      "Check this message in the conversation before sending it again.",
                  }
                : item,
            ),
          }));
          throw new Error(
            "Check this message in the conversation before sending it again.",
          );
        }
        try {
          await this.update((record) => ({
            ...record,
            outbox: record.outbox.map((item) =>
              item.id === message.id
                ? { ...item, state: "sending", error: undefined }
                : item,
            ),
          }));
          if (this.removed.has(message.id) || forgottenHosts.has(this.hostId))
            throw new Error("This saved message was removed");
          const result = await this.host.sendMessage(message);
          if (generation !== this.generation || !this.active)
            throw new Error(
              "Offline storage changed while delivering this message",
            );
          if (result?.action === "pending") {
            const text = messageDraft(message);
            await this.update((record) => {
              const key =
                message.workspace +
                ":" +
                (message.method === "say"
                  ? "orchestrator"
                  : message.params.thread);
              return {
                ...record,
                outbox: record.outbox.filter((item) => item.id !== message.id),
                drafts: {
                  ...record.drafts,
                  [key]: appendDraft(record.drafts[key], text),
                },
              };
            });
            const key =
              message.workspace +
              ":" +
              (message.method === "say"
                ? "orchestrator"
                : message.params.thread);
            return {
              ...result,
              restored_draft: { key, text },
            };
          }
          await this.remove(message.id);
          return result;
        } catch (error) {
          if (
            generation !== this.generation ||
            !this.active ||
            forgottenHosts.has(this.hostId)
          )
            throw error;
          const text = error instanceof Error ? error.message : String(error);
          const uncertain =
            (error as Error & { delivery?: string }).delivery === "rejected" ||
            text.includes("REQUEST_REJECTED:") ||
            /unknown|safe retry|receipt.*(expired|window)|started but|check.*before.*retry/i.test(
              text,
            );
          await this.update((record) => ({
            ...record,
            outbox: record.outbox.map((item) =>
              item.id === message.id
                ? {
                    ...item,
                    state: uncertain ? "review" : "queued",
                    error: text.replace(/REQUEST_REJECTED: /g, ""),
                  }
                : item,
            ),
          }));
          throw error;
        }
      })
      .finally(() => this.inFlight.delete(message.id));
    this.deliveryTail = run;
    this.inFlight.set(message.id, run);
    return run;
  }
  async retire() {
    ++this.generation;
    this.active = false;
    await Promise.allSettled([this.tail, this.loaded]);
  }
  async clear() {
    ++this.generation;
    this.active = false;
    forgottenHosts.add(this.hostId);
    await this.tail.catch(() => {});
    await this.host.secureDelete?.(this.key);
    await this.host.secureDelete?.(this.pendingKey);
    await this.host.secureDelete?.(`bootstrap:${this.hostId}`);
    this.record = blank();
    this.changed();
  }
}
// One journal per hub for this renderer, even when its transport API is replaced.
// Old delivery callbacks and new mounts therefore share one record and write queue.
const journals = new Map<string, OfflineJournal>();
export function offlineJournal(host: DesktopAPI, hostId: string) {
  let journal = journals.get(hostId);
  if (!journal) {
    journal = new OfflineJournal(host, hostId);
    journals.set(hostId, journal);
  } else journal.rebind(host);
  return journal;
}
