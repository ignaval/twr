// The message box under a conversation: the text, its `/` command and `@` thread
// suggestions, the reply suggestion, uploads, voice and the send button. It owns the text
// as you type: the drafts live in a store outside React state (`Drafts`), and only this
// component subscribes to the one on screen, so a keystroke re-renders the box, not the
// whole app. App reads the drafts when it acts on them (sending, slash commands) and
// writes them when something else fills the box (take back, uploads, the outbox).
import {
  useEffect,
  useLayoutEffect,
  useState,
  useSyncExternalStore,
  type FormEvent,
  type RefObject,
} from "react";
import { ArrowUp, ChevronDown, Paperclip, X } from "lucide-react";
import { appendDraft, type OfflineJournal } from "./offline";
import type { UploadController } from "./uploads";
import { takesSuggestion, type useAssistance } from "./assistance";
import { VoiceControl } from "./VoiceControl";
import { Spinner } from "./ui";
import { SendAsLoop } from "./LoopViews";
import type { DesktopAPI, Json, Thread } from "./types";

/**
 * Makes the message field as tall as its text, wrapped lines included: at least its CSS
 * `min-height` (its `rows`), at most its CSS `max-height`, past which the text scrolls
 * inside it. The composer's own box keeps its height while the field is measured, so the
 * conversation above it does not move (one scrolled to its end stays there).
 */
export function fitToText(field: HTMLTextAreaElement | null) {
  if (!field?.isConnected) return;
  const box = field.parentElement;
  const held = box?.style.height ?? "";
  if (box) box.style.height = `${box.getBoundingClientRect().height}px`;
  field.style.height = "";
  // The field's borders (box-sizing: border-box) on top of its text and padding.
  const needed = field.scrollHeight + field.offsetHeight - field.clientHeight;
  if (needed > field.offsetHeight)
    field.style.height = `${Math.ceil(needed)}px`;
  if (box) box.style.height = held;
}

/**
 * The message drafts, one per destination: `<workspace>:orchestrator` or
 * `<workspace>:<thread id>`. Every change replaces the record (what `all()` returned is
 * never changed afterwards) and then calls the subscribers.
 */
export class Drafts {
  private values: Record<string, string> = {};
  private listeners = new Set<() => void>();
  /** The draft for one destination, `""` when there is none. */
  get(key: string): string {
    return this.values[key] || "";
  }
  /** Every destination's draft. */
  all(): Record<string, string> {
    return this.values;
  }
  /** Sets one destination's draft, or changes it from what it was. */
  set(
    key: string,
    value: string | ((old: string | undefined) => string | undefined),
  ) {
    this.update(
      (old) =>
        ({
          ...old,
          [key]: typeof value === "function" ? value(old[key]) : value,
        }) as Record<string, string>,
    );
  }
  /** Replaces the whole record, from what it was. */
  update(change: (old: Record<string, string>) => Record<string, string>) {
    this.values = change(this.values);
    for (const listener of [...this.listeners]) listener();
  }
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
}

/** One destination's draft, re-rendering the caller when it changes. */
export function useDraft(drafts: Drafts, key: string) {
  const read = () => drafts.get(key);
  return useSyncExternalStore(drafts.subscribe, read, read);
}

/** Whether a destination has a draft: re-renders the caller only when that changes. */
export function useHasDraft(drafts: Drafts, key: string) {
  const read = () => !!drafts.get(key);
  return useSyncExternalStore(drafts.subscribe, read, read);
}

/** Which suggestion list is dismissed or chosen in, kept while the box is unmounted
 * (another section is on screen), as it was when App held it. */
export interface ComposerMemory {
  slashHidden: boolean;
  slashIndex: number;
  mentionHidden: boolean;
  mentionIndex: number;
}
export const composerMemory = (): ComposerMemory => ({
  slashHidden: false,
  slashIndex: -1,
  mentionHidden: false,
  mentionIndex: 0,
});

export function Composer({
  drafts,
  draftKey,
  memory,
  input,
  hostApi,
  rpc,
  operate,
  connected,
  journal,
  sending,
  workspace,
  target,
  destination,
  setDestination,
  orchestrator,
  workers,
  slashCommands,
  shellCwd,
  assistance,
  acceptSuggestion,
  onEdit,
  send,
  submitSlash,
  sendAsLoop,
  uploads,
  submitVoice,
  voiceHint,
  spokenLately,
  onDismissKeyboard,
  fail,
}: {
  drafts: Drafts;
  /** The destination on screen: its draft is the box's text. */
  draftKey: string;
  memory: ComposerMemory;
  input: RefObject<HTMLTextAreaElement | null>;
  hostApi: DesktopAPI;
  rpc: (method: string, params?: Json) => Promise<Json>;
  /** This device may send messages (its `operate` scope). */
  operate: boolean;
  connected: boolean;
  journal?: OfflineJournal;
  sending: boolean;
  workspace: string;
  target?: Thread;
  destination?: string;
  setDestination: (thread: string | undefined) => void;
  orchestrator?: Thread;
  /** The threads `@` can reference. */
  workers: Thread[];
  /** The commands `/` suggests. */
  slashCommands: { id: string; title: string }[];
  shellCwd?: string;
  assistance: ReturnType<typeof useAssistance>;
  acceptSuggestion: () => void;
  /** Called on every change typed in the box (it forgets an accepted suggestion). */
  onEdit: () => void;
  send: (event?: FormEvent) => Promise<void>;
  submitSlash: (text: string) => Promise<void>;
  /** Send as implementation loop: the composer's draft is the task. Only while the
   * destination is a thread. */
  sendAsLoop?: () => void;
  uploads: UploadController;
  submitVoice: (text: string) => Promise<Json>;
  voiceHint: string[];
  spokenLately: () => string[];
  onDismissKeyboard?: () => void;
  fail: (error: unknown) => void;
}) {
  const draft = useDraft(drafts, draftKey);
  const [slashHidden, setSlashHidden] = useState(memory.slashHidden);
  const [mentionIndex, setMentionIndex] = useState(memory.mentionIndex);
  const [mentionHidden, setMentionHidden] = useState(memory.mentionHidden);
  const [slashIndex, setSlashIndex] = useState(memory.slashIndex);
  // The field grows and shrinks with its text (fitToText): on every change of the text
  // (typing, sending, a draft put back), of its width (a resized window, a rotated phone)
  // and of the text size.
  useLayoutEffect(() => fitToText(input.current), [draft, input]);
  useEffect(() => {
    const field = input.current;
    if (!field || typeof ResizeObserver === "undefined") return;
    let width = field.clientWidth;
    const refit = () => fitToText(field);
    const resized = new ResizeObserver(() => {
      if (field.clientWidth === width) return;
      width = field.clientWidth;
      refit();
    });
    resized.observe(field);
    window.addEventListener("twr:typography", refit);
    return () => {
      resized.disconnect();
      window.removeEventListener("twr:typography", refit);
    };
  }, [input]);
  useEffect(() => {
    Object.assign(memory, {
      slashHidden,
      slashIndex,
      mentionHidden,
      mentionIndex,
    });
  }, [memory, slashHidden, slashIndex, mentionHidden, mentionIndex]);
  function chooseMention(thread: Thread) {
    drafts.set(draftKey, draft.replace(/@[\w-]*$/, "@" + thread.name + " "));
    setMentionHidden(true);
    input.current?.focus();
  }
  const mentions = draft.match(/(?:^|\s)@([\w-]*)$/)
    ? workers
        .filter((t) => t.name.startsWith(draft.match(/@([\w-]*)$/)?.[1] || ""))
        .slice(0, 6)
    : [];
  const slashMatches =
    draft.startsWith("/") && !draft.startsWith("//")
      ? slashCommands
          .filter((c) => c.id.startsWith(draft.slice(1).split(/\s/)[0]))
          .slice(0, 6)
      : [];
  return (
    <>
      {!draft &&
        assistance.suggestion &&
        document.documentElement.dataset.mobile === "true" && (
          <button className="suggested-message" onClick={acceptSuggestion}>
            <span className="visually-hidden">Suggested reply: </span>
            <span>{assistance.suggestion}</span>
            <span className="visually-hidden">, tap to use</span>
          </button>
        )}
      <form
        className={`composer ${draft.startsWith("!") && !draft.startsWith("!!!") ? "shell-mode" : ""}`}
        aria-label="Message composer"
        onSubmit={(event) => void send(event)}
      >
        <div className="composer-label">
          <span className="destination-chip">
            <span className="destination-dot" aria-hidden="true" />
            To:{" "}
            <b>
              {draft.startsWith("!") && !draft.startsWith("!!!")
                ? "Shell · " +
                  (destination
                    ? target?.name
                    : shellCwd || "workspace folder") +
                  (draft.startsWith("!!") ? " · share result" : "")
                : destination
                  ? target?.name
                  : "Orchestrator"}
            </b>
            {destination && (
              <button
                type="button"
                aria-label="Return to orchestrator"
                title="Return to orchestrator"
                onClick={() => setDestination(undefined)}
              >
                <X size={12} />
              </button>
            )}
          </span>

          <span>
            {!connected
              ? "Disconnected"
              : target
                ? target.status_label || target.status
                : "Choose an orchestrator"}
          </span>
          {onDismissKeyboard && (
            <button
              type="button"
              className="keyboard-dismiss"
              aria-label="Dismiss keyboard"
              onClick={onDismissKeyboard}
            >
              <ChevronDown size={18} />
            </button>
          )}
        </div>
        <textarea
          ref={input}
          aria-label={
            destination
              ? `Message ${target?.name} directly`
              : "Message orchestrator"
          }
          aria-describedby="twr-composer-hints"
          aria-autocomplete={
            mentions.length || slashMatches.length ? "list" : undefined
          }
          aria-controls={
            mentions.length && !mentionHidden
              ? "twr-mentions"
              : slashMatches.length && !slashHidden
                ? "twr-slash-suggestions"
                : undefined
          }
          aria-activedescendant={
            mentions.length && !mentionHidden
              ? `twr-mention-${mentionIndex % mentions.length}`
              : slashMatches.length && !slashHidden && slashIndex >= 0
                ? `twr-slash-${slashIndex % slashMatches.length}`
                : undefined
          }
          value={draft}
          onChange={(e) => {
            onEdit();
            drafts.set(draftKey, e.target.value);
            setSlashHidden(false);
            setSlashIndex(-1);
            setMentionHidden(false);
            setMentionIndex(0);
          }}
          placeholder={
            // A suggestion shows as the empty box's placeholder, as the TUI's
            // ghost text: Tab or → takes it, typing types over it.
            !draft && assistance.suggestion
              ? assistance.suggestion
              : document.documentElement.dataset.mobile === "true"
                ? "Message…"
                : destination
                  ? `Message ${target?.name} directly…`
                  : "Ask your orchestrator to build something…"
          }
          // The empty field's height; fitToText grows it with the text.
          rows={document.documentElement.dataset.mobile === "true" ? 1 : 2}
          disabled={(!connected && !journal) || !operate}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing || e.keyCode === 229) return;
            if (
              takesSuggestion(
                e,
                draft,
                assistance.suggestion,
                assistance.suggestionShownFor(),
              )
            ) {
              e.preventDefault();
              acceptSuggestion();
              return;
            }
            if (mentions.length && !mentionHidden) {
              if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                e.preventDefault();
                setMentionIndex(
                  (i) =>
                    (i + (e.key === "ArrowDown" ? 1 : mentions.length - 1)) %
                    mentions.length,
                );
                return;
              }
              if ((e.key === "Enter" && !e.shiftKey) || e.key === "Tab") {
                e.preventDefault();
                chooseMention(mentions[mentionIndex % mentions.length]);
                return;
              }
              if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                setMentionHidden(true);
                return;
              }
            }
            if (e.key === "Escape" && slashMatches.length && !slashHidden) {
              e.preventDefault();
              e.stopPropagation();
              setSlashHidden(true);
              setSlashIndex(-1);
              return;
            }
            if (
              slashMatches.length &&
              !slashHidden &&
              !draft.includes("\n") &&
              (e.key === "ArrowDown" || e.key === "ArrowUp")
            ) {
              // Arrow keys pick a suggested command; Enter then runs it.
              e.preventDefault();
              setSlashIndex((i) =>
                e.key === "ArrowDown"
                  ? (i + 1) % slashMatches.length
                  : i <= 0
                    ? slashMatches.length - 1
                    : i - 1,
              );
              return;
            }
            if (
              e.key === "Enter" &&
              !e.shiftKey &&
              slashMatches.length &&
              !slashHidden &&
              slashIndex >= 0
            ) {
              e.preventDefault();
              const command = slashMatches[slashIndex % slashMatches.length];
              setSlashIndex(-1);
              void submitSlash(
                "/" + command.id + draft.slice(1).replace(/^\S+/, ""),
              );
              return;
            }
            if (
              e.key === "Enter" &&
              !e.shiftKey &&
              document.documentElement.dataset.mobile !== "true" &&
              !e.nativeEvent.isComposing
            ) {
              e.preventDefault();
              void send();
            }
          }}
        />
        <div className="composer-bottom">
          {hostApi.uploadFile && operate && (
            <label
              className="attachment-button"
              title="Upload files to this thread"
            >
              <Paperclip size={18} />
              <span className="sr-only">Upload files</span>
              <input
                type="file"
                aria-label="Upload files"
                multiple
                disabled={!connected || !target || sending}
                onChange={(event) => {
                  const files = Array.from(event.target.files || []);
                  event.target.value = "";
                  if (target) {
                    try {
                      uploads.add(files, target.id, workspace);
                    } catch (error) {
                      fail(error);
                    }
                  }
                }}
              />
            </label>
          )}
          <span id="twr-composer-hints">
            <kbd>/</kbd> commands <span>·</span> <kbd>@</kbd> reference a thread{" "}
            <span>·</span> Shift Enter for a new line
            {!draft && assistance.suggestion && (
              <span>
                {" "}
                · <kbd>Tab</kbd> or <kbd>→</kbd> takes the suggestion
              </span>
            )}
            {assistance.stats && (
              <small
                title={`${assistance.stats.calls} calls · ${assistance.stats.failures} failed`}
              >
                {" "}
                · {assistance.stats.source} $
                {Number(assistance.stats.cost_usd || 0).toFixed(4)} USD
              </small>
            )}
          </span>
          <div className="composer-actions">
            <VoiceControl
              host={hostApi}
              rpc={rpc}
              connected={connected}
              allowed={operate && Boolean(orchestrator)}
              submit={submitVoice}
              hint={voiceHint}
              spoken={spokenLately}
              edit={(text) => {
                setDestination(undefined);
                drafts.set(workspace + ":orchestrator", (old) =>
                  appendDraft(old, text),
                );
                input.current?.focus();
              }}
              onError={fail}
            />
            <button
              className="send-button"
              type="submit"
              disabled={
                !operate ||
                !draft.trim() ||
                (!connected &&
                  (!journal ||
                    draft.startsWith("/") ||
                    draft.startsWith("!"))) ||
                (!target && !draft.startsWith("/")) ||
                sending
              }
              aria-label={connected ? "Send message" : "Save message for later"}
              title={
                connected
                  ? "Send message"
                  : "Save on this device until the hub reconnects"
              }
            >
              {sending ? <Spinner /> : <ArrowUp size={18} />}
            </button>
            {sendAsLoop && target && !target.is_orchestrator && (
              <SendAsLoop
                thread={target}
                phone={document.documentElement.dataset.mobile === "true"}
                busy={sending}
                onSend={sendAsLoop}
              />
            )}
          </div>
        </div>
        {mentions.length > 0 && !mentionHidden && (
          <div
            className="mentions"
            id="twr-mentions"
            role="listbox"
            aria-label="Threads to reference"
          >
            {mentions.map((t, i) => (
              <button
                className={i === mentionIndex % mentions.length ? "active" : ""}
                type="button"
                role="option"
                id={`twr-mention-${i}`}
                aria-selected={i === mentionIndex % mentions.length}
                tabIndex={-1}
                key={t.id}
                onClick={() => chooseMention(t)}
              >
                {" "}
                @{t.name}
              </button>
            ))}
          </div>
        )}
        {slashMatches.length > 0 && !slashHidden && (
          <div
            className="mentions slash-suggestions"
            id="twr-slash-suggestions"
            role="listbox"
            aria-label="Commands"
          >
            {slashMatches.map((command, i) => (
              <button
                type="button"
                role="option"
                id={`twr-slash-${i}`}
                className={
                  slashIndex >= 0 && i === slashIndex % slashMatches.length
                    ? "active"
                    : ""
                }
                aria-selected={
                  slashIndex >= 0 && i === slashIndex % slashMatches.length
                }
                tabIndex={-1}
                key={command.id}
                onClick={() =>
                  submitSlash(
                    "/" + command.id + draft.slice(1).replace(/^\S+/, ""),
                  )
                }
              >
                <b>/{command.id}</b>
                <span className="visually-hidden">, </span>
                <span>{command.title}</span>
              </button>
            ))}
          </div>
        )}
      </form>
    </>
  );
}
