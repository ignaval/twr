import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { ArrowDown, ArrowUp, Search } from "lucide-react";
import type { Attention, HubEvent, InboxSource, Json, Thread } from "./types";
import { Dialog, Empty, Markdown, Spinner } from "./ui";
import { requestLine } from "./loops";
import { errorMessage } from "./errors";
import { announce, scrollBehavior } from "./a11y";
import "./inbox-search.css";

export type InboxFilter =
  "all" | "permission" | "question" | "completion" | "other";
export function inboxCategory(item: Attention): Exclude<InboxFilter, "all"> {
  // A loop's setup card and a stuck loop's item ask the user to choose, as questions do.
  if (item.loop_setup || item.loop_stuck) return "question";
  if (item.reason === "permission" || item.reason === "question")
    return item.reason;
  return item.reason === "review" ? "completion" : "other";
}
const filters: [InboxFilter, string][] = [
  ["all", "All"],
  ["permission", "Permissions"],
  ["question", "Questions"],
  ["completion", "Completions"],
  ["other", "Other"],
];
export function NeedsYouInbox({
  source,
  sources = [],
  renderItem,
  onOpenSource,
  refreshing,
  onRefresh,
}: {
  source: InboxSource;
  sources?: InboxSource[];
  renderItem: (item: Attention) => ReactNode;
  onOpenSource: (source: InboxSource) => void;
  refreshing?: boolean;
  onRefresh?: () => void;
}) {
  const [filter, setFilter] = useState<InboxFilter>("all");
  const [active, setActive] = useState("");
  const root = useRef<HTMLDivElement>(null);
  const groups = useMemo(() => {
    const unique = new Map(
      sources.map((item) => [item.hubId + ":" + item.workspace, item]),
    );
    const sourceKey = source.hubId + ":" + source.workspace;
    const saved = unique.get(sourceKey);
    unique.set(
      sourceKey,
      source.cached &&
        source.provisional &&
        !source.attention.length &&
        saved?.attention.length
        ? { ...saved, cached: true }
        : source,
    );
    return [...unique.values()].map((group) => ({
      ...group,
      attention: group.attention.filter(
        (item) => filter === "all" || inboxCategory(item) === filter,
      ),
    }));
  }, [source, sources, filter]);
  const current = groups.find(
    (group) =>
      group.hubId === source.hubId && group.workspace === source.workspace,
  )!;
  const keys =
    current.cached || current.error || current.running === false
      ? []
      : current.attention.map((item) => item.attn_id + ":" + item.revision);
  const selected = keys.indexOf(active);
  const lastIndex = useRef(0);
  if (selected >= 0) lastIndex.current = selected;
  function move(delta: number) {
    const index =
      selected < 0
        ? delta > 0
          ? Math.min(lastIndex.current, keys.length - 1)
          : (lastIndex.current - 1 + keys.length) % keys.length
        : (selected + delta + keys.length) % keys.length;
    const key = keys[index];
    if (!key) return;
    setActive(key);
    const node = [
      ...(root.current?.querySelectorAll<HTMLElement>("[data-inbox-key]") ||
        []),
    ].find((element) => element.dataset.inboxKey === key);
    node?.scrollIntoView({ block: "center", behavior: scrollBehavior() });
    node
      ?.querySelector<HTMLElement>(".attention-card")
      ?.focus({ preventScroll: true });
  }
  useEffect(() => {
    const navigate = (event: KeyboardEvent) => {
      if (
        (event.target as HTMLElement)?.closest?.(
          "input,textarea,select,[contenteditable]",
        ) ||
        event.altKey ||
        event.metaKey ||
        event.ctrlKey ||
        document.querySelector('[role="dialog"]')
      )
        return;
      if (["j", "k", "ArrowDown", "ArrowUp"].includes(event.key)) {
        if (!keys.length) return;
        event.preventDefault();
        move(event.key === "j" || event.key === "ArrowDown" ? 1 : -1);
      }
    };
    window.addEventListener("keydown", navigate);
    return () => window.removeEventListener("keydown", navigate);
  });
  const count = groups.reduce((sum, group) => sum + group.attention.length, 0);
  return (
    <div className="manager needs-inbox" ref={root}>
      <header className="section-heading">
        <div>
          <span className="eyebrow">Your attention</span>
          <h1>
            Needs you <span className="heading-count">{count}</span>
            <span className="visually-hidden">
              {" "}
              {count === 1 ? "request" : "requests"}
            </span>
          </h1>
          <p>
            Review requests by hub and workspace. J / K moves between current
            requests.
          </p>
        </div>
      </header>
      <div className="inbox-toolbar">
        <div className="inbox-filters" role="group" aria-label="Request type">
          {filters.map(([key, label]) => (
            <button
              key={key}
              aria-pressed={filter === key}
              onClick={() => {
                setFilter(key);
                setActive("");
                lastIndex.current = 0;
              }}
            >
              {label}
            </button>
          ))}
        </div>
        <div
          className="inbox-navigation"
          role="group"
          aria-label="Move between requests"
        >
          <button
            aria-label="Previous request"
            disabled={!keys.length}
            onClick={() => move(-1)}
          >
            <ArrowUp size={16} />
            Previous
          </button>
          <button
            aria-label="Next request"
            disabled={!keys.length}
            onClick={() => move(1)}
          >
            Next
            <ArrowDown size={16} />
          </button>
          {onRefresh && (
            <button disabled={refreshing} onClick={onRefresh}>
              {refreshing ? <Spinner /> : null}Refresh
            </button>
          )}
        </div>
      </div>
      {groups.map((group) => {
        if (group.scanError)
          return (
            <p key="scan-error" role="status" className="muted">
              {group.error}
            </p>
          );
        const live =
          group.hubId === source.hubId &&
          group.workspace === source.workspace &&
          !group.cached &&
          !group.error &&
          group.running !== false;
        return (
          <section
            key={group.hubId + ":" + group.workspace}
            className="inbox-group"
            aria-labelledby={`inbox-${group.hubId}-${group.workspace}`}
          >
            <header>
              <h2 id={`inbox-${group.hubId}-${group.workspace}`}>
                {group.hubName} <span> / {group.workspace}</span>
              </h2>
              <span>
                {group.running === false
                  ? "Not running"
                  : group.error
                    ? "Could not refresh"
                    : group.cached
                      ? `Saved summary${group.updatedAt ? " · " + new Date(group.updatedAt).toLocaleString() : ""} · reconnect to review`
                      : live
                        ? "Live"
                        : "Open workspace to review"}
              </span>
            </header>
            {(group.running === false ||
              (group.error &&
                !(
                  group.hubId === source.hubId &&
                  group.workspace === source.workspace
                ))) &&
              !group.attention.length && (
                <button onClick={() => onOpenSource(group)}>
                  Open workspace
                </button>
              )}
            {group.error && (
              <p role="status" className="muted">
                {group.error}
              </p>
            )}
            {group.attention.length > 0 && (
              <div
                className="inbox-items"
                role="list"
                aria-label={`Requests in ${group.workspace}`}
              >
                {group.attention.map((item) =>
                  live ? (
                    <div
                      role="listitem"
                      key={item.attn_id + ":" + item.revision}
                      data-inbox-key={item.attn_id + ":" + item.revision}
                      onFocus={() =>
                        setActive(item.attn_id + ":" + item.revision)
                      }
                    >
                      {renderItem(item)}
                    </div>
                  ) : (
                    <div role="listitem" key={item.attn_id}>
                      <button
                        className="inbox-source-request"
                        onClick={() => onOpenSource(group)}
                      >
                        <b>
                          {group.threads.find(
                            (thread) => thread.id === item.thread,
                          )?.name || item.thread}
                        </b>
                        <span className="visually-hidden">, </span>
                        <span>
                          {inboxCategory(item)} · {requestLine(item)}
                        </span>
                        <span className="visually-hidden">. </span>
                        <small>Open {group.workspace}</small>
                      </button>
                    </div>
                  ),
                )}
              </div>
            )}
            {!group.error &&
              group.running !== false &&
              !group.attention.length && (
                <p className="muted">
                  No {filter === "all" ? "pending" : filter} requests
                  {group.cached
                    ? " in this saved summary. Reconnect to check for new requests."
                    : "."}
                </p>
              )}
          </section>
        );
      })}
      {!count &&
        !groups.some(
          (group) => group.error || group.cached || group.running === false,
        ) && (
          <Empty
            title="You’re all caught up"
            description="Questions, permissions, and completed work that needs review appear here."
          />
        )}
    </div>
  );
}
interface SearchHit {
  thread_name?: string;
  event_id: string;
  thread: string;
  seq: number;
  kind: string;
  ts: string;
  excerpt: string;
}
interface SearchPage {
  items: SearchHit[];
  next?: string;
  more: boolean;
  workspace: string;
}
export function eventText(event: HubEvent): string {
  const data = event.data || {};
  if (event.kind === "conversation.update")
    return [data.user_message, data.text].filter(Boolean).join("\n\n");
  if (event.kind === "agent.tool_call")
    return [data.tool, data.input_summary].filter(Boolean).join("\n\n");
  if (event.kind === "agent.tool_result")
    return data.output_summary || JSON.stringify(data, null, 2);
  return (
    data.user_message ||
    data.text ||
    data.prompt ||
    data.summary ||
    data.message ||
    JSON.stringify(data, null, 2)
  );
}
export function ConversationSearch({
  rpc,
  workspace,
  hubName,
  threads,
  onClose,
}: {
  rpc: (method: string, params?: Json) => Promise<Json>;
  workspace: string;
  hubName: string;
  threads: Thread[];
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [thread, setThread] = useState("");
  const [page, setPage] = useState<SearchPage>();
  const [lastAdded, setLastAdded] = useState<number>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [chosen, setChosen] = useState<SearchHit>();
  const [context, setContext] = useState<{
    event: HubEvent;
    events: HubEvent[];
  }>();
  const generation = useRef(0);
  const contextGeneration = useRef(0);
  const searchInput = useRef<HTMLInputElement>(null);
  const backButton = useRef<HTMLButtonElement>(null);
  const returnTo = useRef<string | undefined>(undefined);
  useEffect(() => {
    // Keep focus inside the dialog when its content is swapped.
    if (chosen) backButton.current?.focus();
    else if (returnTo.current) {
      const hit = document.querySelector<HTMLElement>(
        `[data-search-hit="${CSS.escape(returnTo.current)}"]`,
      );
      (hit || searchInput.current)?.focus();
      returnTo.current = undefined;
    }
  }, [chosen]);
  useEffect(() => {
    const id = ++generation.current;
    setPage(undefined);
    setLastAdded(undefined);
    setError("");
    if (!query.trim()) {
      setBusy(false);
      return;
    }
    setBusy(true);
    const timer = setTimeout(() => {
      void rpc("conversation.search", {
        query: query.trim(),
        ...(thread ? { thread } : {}),
        limit: 30,
      })
        .then((result) => {
          if (result.workspace !== workspace)
            throw new Error("Search workspace changed. Run the search again.");
          if (id === generation.current) {
            setPage(result);
            setLastAdded(result.items.length);
            if (result.items.length)
              announce(
                `${result.items.length}${result.more ? " or more" : ""} matching ${result.items.length === 1 && !result.more ? "event" : "events"}`,
              );
          }
        })
        .catch((failure) => {
          if (id === generation.current) setError(errorMessage(failure));
        })
        .finally(() => {
          if (id === generation.current) setBusy(false);
        });
    }, 250);
    return () => {
      clearTimeout(timer);
      generation.current++;
    };
  }, [query, thread, rpc, workspace]);
  useEffect(
    () => () => {
      contextGeneration.current++;
    },
    [],
  );
  async function more() {
    if (!page?.more || !page.next || busy) return;
    const id = generation.current;
    setBusy(true);
    try {
      const result: SearchPage = await rpc("conversation.search", {
        query: query.trim(),
        ...(thread ? { thread } : {}),
        before: page.next,
        limit: 30,
      });
      if (result.workspace !== workspace)
        throw new Error("Search workspace changed. Run the search again.");
      if (id === generation.current) {
        setLastAdded(result.items.length);
        setPage({ ...result, items: [...page.items, ...result.items] });
        if (result.items.length)
          announce(`${result.items.length} more matching events loaded`);
      }
    } catch (failure) {
      if (id === generation.current) setError(errorMessage(failure));
    } finally {
      if (id === generation.current) setBusy(false);
    }
  }
  async function open(hit: SearchHit) {
    const id = ++contextGeneration.current;
    setChosen(hit);
    setContext(undefined);
    setError("");
    try {
      const result = await rpc("thread.event_context", {
        thread: hit.thread,
        event_id: hit.event_id,
        query: query.trim(),
        before: 12,
        after: 12,
      });
      if (
        result.workspace !== workspace ||
        result.event?.id !== hit.event_id ||
        result.thread !== hit.thread
      )
        throw new Error(
          "The selected event belongs to another conversation. Run the search again.",
        );
      if (id === contextGeneration.current) setContext(result);
    } catch (failure) {
      if (id === contextGeneration.current) setError(errorMessage(failure));
    }
  }
  return (
    <Dialog
      title={chosen ? "Conversation event" : "Search conversations"}
      subtitle={`${hubName} / ${workspace}`}
      wide
      onClose={onClose}
    >
      {chosen ? (
        <div className="search-context">
          <button
            ref={backButton}
            aria-label="Back to search results"
            onClick={() => {
              returnTo.current = chosen.event_id;
              contextGeneration.current++;
              setChosen(undefined);
              setContext(undefined);
              setError("");
            }}
          >
            ← Search results
          </button>
          <p>
            {threads.find((item) => item.id === chosen.thread)?.name ||
              chosen.thread_name ||
              chosen.thread}{" "}
            · event {chosen.seq}
          </p>
          <blockquote className="search-match">{chosen.excerpt}</blockquote>
          {!context && !error && <Spinner />}
          <div aria-busy={!context && !error ? true : undefined}>
            {context?.events.map((event) => (
              <article
                key={event.id}
                className={
                  event.id === context.event.id
                    ? "search-event selected"
                    : "search-event"
                }
                ref={
                  event.id === context.event.id
                    ? (node) => {
                        node?.scrollIntoView({ block: "center" });
                      }
                    : undefined
                }
              >
                <header>
                  <b>{event.kind}</b>
                  <time>{new Date(event.ts).toLocaleString()}</time>
                  <span>#{event.seq}</span>
                </header>
                <Markdown text={eventText(event)} />
              </article>
            ))}
          </div>
        </div>
      ) : (
        <div className="conversation-search">
          <label className="conversation-search-input">
            <Search size={18} />
            <input
              ref={searchInput}
              aria-label="Search messages, tool output, and decisions"
              placeholder="Messages, tool output, decisions…"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              autoFocus
            />
          </label>
          <label className="search-thread-filter">
            Conversation
            <select
              value={thread}
              onChange={(event) => setThread(event.target.value)}
            >
              <option value="">All conversations in {workspace}</option>
              {threads.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </select>
          </label>
          <p className="muted">
            Searches saved events in this workspace, including archived output.
            Selecting a result preserves your message destination.
          </p>
          <div className="search-results">
            {page?.items.map((hit) => (
              <button
                key={hit.event_id}
                data-search-hit={hit.event_id}
                className="search-hit"
                onClick={() => void open(hit)}
              >
                <span>
                  <b>
                    {threads.find((item) => item.id === hit.thread)?.name ||
                      hit.thread_name ||
                      hit.thread}
                  </b>
                  <small>
                    {hit.kind} · #{hit.seq}
                  </small>
                </span>
                <p>{hit.excerpt}</p>
                <time>{new Date(hit.ts).toLocaleString()}</time>
              </button>
            ))}
          </div>
          {busy && <Spinner />}
          {page && !page.items.length && !page.more && (
            <p role="status">No matching events.</p>
          )}
          {page && (lastAdded === 0 || !page.items.length) && page.more && (
            <p role="status">
              No matches in these events. Search older events to continue.
            </p>
          )}
          {page?.more && (
            <button onClick={() => void more()} disabled={busy}>
              Search older events
            </button>
          )}
        </div>
      )}
      {error && (
        <p role="alert" className="error-text">
          {error}
        </p>
      )}
    </Dialog>
  );
}
