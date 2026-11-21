import type { DesktopAPI, HubEvent, Json, ViewState } from "./types";
import { requestId } from "./remote-crypto";
import { splitAttachments, type NamedFile } from "./attachmentText";

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
/**
 * `text`, a draft (a saved one, one taken back), added to the box's `current` draft. After
 * other text it goes as the message it sends ([`messageText`]): Send reads escapes only at
 * the start of the box, so a `\!` or `//` that kept it a message there would be sent too.
 */
/**
 * An earlier message the outbox moved back to its draft (the hub held it for a waiting
 * request) is in the saved drafts already, but not yet in the message box: when a later
 * delivery fails, the error carries it (`restored_drafts`), so the caller puts it in the box
 * as a success would. Otherwise the box's next save would write over it.
 */
function withRestored(error: unknown, restored: Restored[]) {
  if (!restored.length) return error;
  // A new error for each caller: callers waiting on the same delivery share its error, and
  // each carries what its own delivery put back. The failure itself is its `cause`.
  const carried = new Error(
    error instanceof Error ? error.message : String(error ?? "Not sent"),
    { cause: error },
  ) as Error & { restored_drafts?: Restored[] };
  carried.restored_drafts = [...restoredOf(error), ...restored];
  return carried;
}
/** The drafts a failed [`OfflineJournal.deliver`] had already moved back ([`withRestored`]). */
export function restoredOf(error: unknown): Restored[] {
  const carried = (error as { restored_drafts?: unknown } | null)
    ?.restored_drafts;
  return Array.isArray(carried) ? carried : [];
}
/** A saved message the outbox put back in its draft: its outbox id, its draft's key and its
 * draft. */
export interface Restored {
  id: string;
  key: string;
  text: string;
}
export function appendDraft(current: string | undefined, text: string) {
  if (!current) return text;
  const message = messageText(text);
  if (current === text || current.endsWith("\n\n" + message)) return current;
  // A draft that is a `/` command or a `!` shell line would send none of the text added
  // after it (Send runs the command and clears the box): it becomes a message too.
  const head = isCommandDraft(current) ? literalDraft(current) : current;
  return head + "\n\n" + message;
}
/** A draft Send runs as a `/` command (`//` escapes a leading `/`). */
export function isSlashDraft(draft: string) {
  return draft.startsWith("/") && !draft.startsWith("//");
}
/** A draft Send runs as a `!` shell line (`!!` shares its result; `\!`, or the apps'
 * `!!!`, sends a message that starts with `!`). */
export function isShellDraft(draft: string) {
  return draft.startsWith("!") && !draft.startsWith("!!!");
}
/**
 * A draft Send runs as a `/` command or a `!` shell line rather than sending it: it takes
 * no files and can't be saved offline. Every check of a draft's kind goes through these
 * three, so an escaped message (`//…`, `\!…`, as Take back puts one back) is a message
 * everywhere.
 */
export function isCommandDraft(draft: string) {
  return isSlashDraft(draft) || isShellDraft(draft);
}
/**
 * The message a composer draft sends, as the TUI's `message_text` reads a line: `//...`
 * sends `/...` and `\!...` sends `!...` (one backslash of those before a `!` goes);
 * anything else as it is. (A draft that starts with a single `/` or a `!` is a command, or a
 * shell line, and sends no message.)
 */
export function messageText(draft: string) {
  if (draft.startsWith("\\") && draft.replace(/^\\+/, "").startsWith("!"))
    return draft.slice(1);
  return draft.startsWith("//") ? draft.slice(1) : draft;
}
/**
 * The draft that sends `message` as it is ([`messageText`]'s inverse), as the TUI's
 * `literal_line` writes it: what Take back, Put back and the outbox put back in the box,
 * so Send sends it again as a message, never as a `/` command or a `!` shell line.
 */
export function literalDraft(message: string) {
  if (message.replace(/^\\+/, "").startsWith("!")) return "\\" + message;
  return message.startsWith("/") ? "/" + message : message;
}
/** [`appendDraft`] for a message as sent: escaped ([`literalDraft`]) when it starts the
 * draft, where a `/` or `!` would make it a command; as it is after other text. */
export function appendMessage(current: string | undefined, message: string) {
  return appendDraft(current, literalDraft(message));
}
export function messageDraft(message: OutboxMessage) {
  if (typeof message.draft === "string") return message.draft;
  return literalDraft(
    String(message.params.prompt || message.params.message || ""),
  );
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
  /** Drafts containing unreviewed speech; saved atomically with their text. */
  spokenDrafts?: Record<string, boolean>;
  /**
   * The uploaded files on each draft's conversation, by draft key: names and paths, saved
   * with the text, so a restart puts them back in the box. Optional, like `spokenDrafts`:
   * a record without it (an older app's) loads with none, and an older app reading this
   * one keeps the text and drops the files.
   */
  draftFiles?: Record<string, NamedFile[]>;
  workspaces: Record<string, CachedWorkspace>;
  outbox: OutboxMessage[];
}
import { RECEIPT_WINDOW } from "./offline-constants";
export { RECEIPT_WINDOW } from "./offline-constants";
const HISTORY_WINDOW = 7 * 24 * 60 * 60 * 1000;
const bytes = (value: unknown) =>
  new TextEncoder().encode(JSON.stringify(value)).byteLength;
// Saved objects are replaced, never changed in place: sizes are measured once per object.
const sizes = new WeakMap<object, number>();
function sized(value: unknown): number {
  if (!value || typeof value !== "object") return bytes(value ?? null);
  let size = sizes.get(value);
  if (size === undefined) {
    size = Array.isArray(value)
      ? 2 + value.reduce((sum: number, item) => sum + sized(item) + 1, 0)
      : bytes(value);
    sizes.set(value, size);
  }
  return size;
}
/** Approximate JSON size of a cached workspace, from the cached sizes of its parts. */
function workspaceSize(entry: CachedWorkspace): number {
  let size = sizes.get(entry);
  if (size === undefined) {
    size =
      64 +
      sized(entry.view) +
      sized(entry.transcriptTimes || {}) +
      Object.entries(entry.transcripts).reduce(
        (sum, [id, events]) => sum + id.length + 4 + sized(events),
        0,
      );
    sizes.set(entry, size);
  }
  return size;
}
/** The size checks below: each lane is measured by itself, history from cached sizes. */
export function recordSize(record: OfflineRecord) {
  const pending = bytes({
    drafts: record.drafts,
    spokenDrafts: record.spokenDrafts,
    draftFiles: record.draftFiles,
    outbox: record.outbox,
  });
  const history = Object.entries(record.workspaces).reduce(
    (sum, [key, entry]) => sum + key.length + 4 + workspaceSize(entry),
    0,
  );
  return { pending, history, total: pending + history };
}
export const HISTORY_BOUND = 4 * 1024 * 1024;
export const RECORD_BOUND = 6 * 1024 * 1024;
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
/**
 * Saved draft files that are well formed: a list of names and paths per key, each path
 * once. All of them, however many: a message taken back may put a conversation over the
 * box's limit for new files (`MAX_ATTACHMENTS`), and what it put back stays.
 */
function draftFilesOf(value: unknown): Record<string, NamedFile[]> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>)
      .map(([key, files]): [string, NamedFile[]] => {
        const seen = new Set<string>();
        return [
          key,
          Array.isArray(files)
            ? files
                .filter(
                  (file): file is NamedFile =>
                    !!file &&
                    typeof file.name === "string" &&
                    typeof file.path === "string" &&
                    !!file.path &&
                    !seen.has(file.path) &&
                    !!seen.add(file.path),
                )
                .map(({ name, path }) => ({ name, path }))
            : [],
        ];
      })
      .filter(([, files]) => files.length),
  );
}
/**
 * `text` taken back into a draft, split as the app's box holds it: the text without its
 * `Uploaded file:` lines into the draft, the files it names onto the conversation's saved
 * files (once each).
 */
function backToDraft(
  record: OfflineRecord,
  key: string,
  text: string,
): Pick<OfflineRecord, "drafts" | "draftFiles"> {
  const { body, files } = splitAttachments(text);
  if (!files.length)
    return {
      drafts: {
        ...record.drafts,
        [key]: appendDraft(record.drafts[key], text),
      },
      draftFiles: record.draftFiles,
    };
  const saved = record.draftFiles?.[key] || [];
  const known = new Set(saved.map((file) => file.path));
  return {
    drafts: body.trim()
      ? { ...record.drafts, [key]: appendDraft(record.drafts[key], body) }
      : record.drafts,
    draftFiles: {
      ...record.draftFiles,
      [key]: [...saved, ...files.filter((file) => !known.has(file.path))],
    },
  };
}
/**
 * Drafts as the box holds them: an older app wrote a file's `Uploaded file:` pair into its
 * draft, so each such pair (an upload's path, as `splitAttachments` reads it) moves out of
 * the text onto the conversation's files, once each. Drafts without one stay as they are.
 */
export function draftsWithFiles(
  drafts: Record<string, string>,
  draftFiles: Record<string, NamedFile[]> = {},
): Pick<OfflineRecord, "drafts"> & {
  draftFiles: Record<string, NamedFile[]>;
} {
  let files = draftFiles;
  const out = { ...drafts };
  for (const [key, text] of Object.entries(drafts)) {
    const { body, files: named } = splitAttachments(text);
    if (!named.length) continue;
    const saved = files[key] || [];
    const known = new Set(saved.map((file) => file.path));
    out[key] = body;
    files = {
      ...files,
      [key]: [
        ...saved,
        ...named.filter(
          (file) => !known.has(file.path) && known.add(file.path),
        ),
      ],
    };
  }
  return { drafts: out, draftFiles: files };
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
  const { drafts, draftFiles } = draftsWithFiles(
    Object.fromEntries(
      Object.entries(record.drafts).filter(
        (entry): entry is [string, string] => typeof entry[1] === "string",
      ),
    ),
    draftFilesOf(record.draftFiles),
  );
  return {
    version: 1,
    drafts,
    spokenDrafts: Object.fromEntries(
      Object.entries(record.spokenDrafts || {})
        .filter(([key, value]) => value === true && drafts[key])
        .map(([key]) => [key, true]),
    ),
    draftFiles,
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
          spokenDrafts: pending.spokenDrafts,
          draftFiles: pending.draftFiles,
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
            JSON.stringify({
              ...history,
              drafts: {},
              spokenDrafts: {},
              draftFiles: {},
              outbox: [],
            }),
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
        // Drafts are saved as you type: only that lane is serialized for its size, the
        // cached history's size comes from sizes measured when it was saved.
        let size = recordSize(next);
        while (
          size.total > HISTORY_BOUND &&
          Object.keys(next.workspaces).length
        ) {
          const oldest = Object.entries(next.workspaces).sort(
            (a, b) => a[1].savedAt - b[1].savedAt,
          )[0][0];
          const { [oldest]: _, ...workspaces } = next.workspaces;
          next = { ...next, workspaces };
          size = recordSize(next);
        }
        if (size.total > RECORD_BOUND)
          throw new Error(
            "Offline drafts and messages are full. Remove an outbox item before adding more.",
          );
        const payload =
          lane === "history"
            ? {
                ...next,
                drafts: {},
                spokenDrafts: {},
                draftFiles: {},
                outbox: [],
              }
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
  drafts(
    drafts: Record<string, string>,
    epoch = this.generation,
    spokenDrafts?: Record<string, boolean>,
    /** Every conversation's uploaded files (`UploadController.committed`); left out, the
     * saved ones stay. */
    draftFiles?: Record<string, NamedFile[]>,
  ) {
    if (epoch !== this.generation)
      return Promise.reject(
        new Error("Offline storage changed before saving this draft"),
      );
    return this.update((record) => {
      let next: OfflineRecord = {
        ...record,
        drafts,
        spokenDrafts: spokenDrafts ?? record.spokenDrafts,
        draftFiles: draftFiles ?? record.draftFiles,
      };
      // A message put back that the box has not taken yet ([`claim`]) stays, so a
      // save from the box as it was (as the app closes, say) never writes over it.
      for (const restored of this.unacknowledged)
        next = { ...next, ...backToDraft(next, restored.key, restored.text) };
      return next;
    });
  }
  /**
   * Messages the outbox put back in their drafts (the hub held them for a waiting request)
   * that the message box has not taken yet, by their outbox id: [`drafts`] keeps them until
   * it has, and [`claim`] gives each to the box once. A reset forgets them.
   */
  private unacknowledged: Restored[] = [];
  /**
   * The message box takes the messages a delivery put back (its `restored_draft`, its
   * `restored_drafts`, [`restoredOf`] its error): only those not taken yet, each once, and
   * none from before a reset. Two deliveries waiting on the same message both carry it, and
   * one from storage since reset carries one that is gone: the box applies only what this
   * returns, so a message never comes back twice, nor after a reset.
   */
  claim(restored: readonly Restored[]): Restored[] {
    const taken: Restored[] = [];
    for (const item of restored) {
      const at = this.unacknowledged.findIndex((u) => u.id === item.id);
      if (at < 0) continue;
      taken.push(this.unacknowledged[at]);
      this.unacknowledged.splice(at, 1);
    }
    return taken;
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
          .filter((event) => sized(event) <= 128 * 1024);
        for (const id of Object.keys(transcripts)
          .sort((a, b) => (transcriptTimes[a] || 0) - (transcriptTimes[b] || 0))
          .slice(0, Math.max(0, Object.keys(transcripts).length - 8)))
          if (id !== thread) delete transcripts[id];
      }
      // Never cache credentials/configuration or full tool payloads above the bound.
      const compact: ViewState = {
        ...view,
        threads: view.threads
          .filter((item) => sized(item) <= 128 * 1024)
          .slice(0, 200),
        attention: view.attention.filter((item) => sized(item) <= 128 * 1024),
        decisions: view.decisions
          .filter((item) => sized(item) <= 64 * 1024)
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
      // The files the message names leave its draft with the text. Only those: another
      // message on its way (a hands-free one) may hold the others, and they stay saved
      // until it takes them.
      const sent = new Set(
        splitAttachments(messageDraft(message)).files.map((file) => file.path),
      );
      const { [key]: saved = [], ...draftFiles } = record.draftFiles || {};
      const kept = saved.filter((file) => !sent.has(file.path));
      return {
        ...record,
        drafts: { ...record.drafts, [key]: "" },
        draftFiles: kept.length ? { ...draftFiles, [key]: kept } : draftFiles,
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
        ...backToDraft(record, key, messageDraft(message)),
      };
    });
  }
  async reset() {
    ++this.generation;
    this.active = false;
    this.unacknowledged = [];
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
    const restored: Restored[] = [];
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
        if (current && current.state !== "review")
          throw withRestored(error, restored);
      }
    }
    let result: Json;
    try {
      result = await this.deliverOne(requested, retryReview);
    } catch (error) {
      throw withRestored(error, restored);
    }
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
                ...backToDraft(record, key, text),
              };
            });
            const key =
              message.workspace +
              ":" +
              (message.method === "say"
                ? "orchestrator"
                : message.params.thread);
            const restored: Restored = { id: message.id, key, text };
            this.unacknowledged.push(restored);
            return { ...result, restored_draft: restored };
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
