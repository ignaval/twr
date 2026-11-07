import { useEffect, useState } from "react";
import {
  RECEIPT_WINDOW,
  type OfflineJournal,
  type OutboxMessage,
} from "./offline";
import type { Json } from "./types";
import { useHostAPI } from "./host";

export function Outbox({
  journal,
  workspace,
  connected,
  onSent,
  onCopy,
}: {
  journal?: OfflineJournal;
  workspace: string;
  connected: boolean;
  onSent(result: Json): void;
  onCopy(message: OutboxMessage): void;
}) {
  const host = useHostAPI();
  const [, update] = useState(0);
  const [error, setError] = useState("");
  useEffect(() => journal?.subscribe(() => update((n) => n + 1)), [journal]);
  useEffect(() => {
    if (!journal || !connected) return;
    let cancelled = false;
    let draining = false;
    const drain = async () => {
      if (cancelled || draining) return;
      draining = true;
      try {
        for (const message of journal.snapshot.outbox.filter(
          (item) => item.workspace === workspace && item.state === "queued",
        )) {
          if (cancelled) return;
          try {
            const result = await journal.deliver(message);
            onSent(result);
          } catch {
            const current = journal.snapshot.outbox.find(
              (item) => item.id === message.id,
            );
            if (current && current.state !== "review") break;
          }
        }
      } finally {
        draining = false;
      }
    };
    void drain();
    const retry = setInterval(() => void drain(), 3000);
    return () => {
      cancelled = true;
      clearInterval(retry);
    };
  }, [journal, connected, workspace]);
  const messages =
    journal?.snapshot.outbox.filter((item) => item.workspace === workspace) ||
    [];
  return messages.length ? (
    <section className="outbox" aria-labelledby="twr-outbox-title">
      <h3 id="twr-outbox-title">
        Outbox <span>{messages.length}</span>
        <span className="visually-hidden"> saved messages</span>
      </h3>
      {error && <p role="status">{error}</p>}
      {messages.map((message) => (
        <div key={message.id} className="outbox-message">
          <b>
            {message.state === "review"
              ? "Review delivery"
              : message.state === "sending"
                ? "Sending…"
                : connected
                  ? "Waiting to send"
                  : "Saved on this device"}
          </b>
          <p>
            {String(message.params.prompt || message.params.message).slice(
              0,
              300,
            )}
          </p>
          {String(message.params.prompt || message.params.message).length >
            300 && (
            <details>
              <summary>Full message</summary>
              <p>{String(message.params.prompt || message.params.message)}</p>
            </details>
          )}
          <small>
            {message.method === "thread.send"
              ? `To thread ${message.params.thread}`
              : "To orchestrator"}{" "}
            · {new Date(message.createdAt).toLocaleString()}
          </small>
          {message.error && (
            <p className="outbox-error" role="status">
              {message.error}
            </p>
          )}
          <div>
            <button
              onClick={() =>
                void host
                  .copyText(
                    String(message.params.prompt || message.params.message),
                  )
                  .catch((error) => setError(String(error)))
              }
            >
              Copy message
            </button>
            {Date.now() - message.createdAt < RECEIPT_WINDOW && (
              <button
                disabled={!connected || message.state === "sending"}
                onClick={() =>
                  void journal!
                    .deliver(message, true)
                    .then(onSent)
                    .catch(() => {})
                }
              >
                Retry with same ID
              </button>
            )}
            <button
              disabled={message.state === "sending"}
              onClick={() =>
                (message.state !== "review" ||
                  window.confirm(
                    "This message may already have reached the hub. Check its conversation before sending an edited copy. Move it to the original draft?",
                  )) &&
                void journal!
                  .moveToDraft(message.id)
                  .then(() => {
                    setError("");
                    onCopy(message);
                  })
                  .catch((error) => setError(String(error)))
              }
            >
              Move to original draft
            </button>
            <button
              disabled={message.state === "sending"}
              onClick={() => {
                if (
                  window.confirm(
                    "Remove this saved message? It may already have reached the hub. This does not take it back from the thread.",
                  )
                )
                  void journal!
                    .remove(message.id)
                    .catch((error) => setError(String(error)));
              }}
            >
              Remove from outbox
            </button>
          </div>
        </div>
      ))}
    </section>
  ) : null;
}
