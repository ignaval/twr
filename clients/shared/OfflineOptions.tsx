import { useEffect, useState } from "react";
import { useHostAPI } from "./host";
import { offlineJournal, type OfflineJournal } from "./offline";
import { errorMessage } from "./errors";
import { Dialog } from "./ui";

export function OfflineOptions() {
  const host = useHostAPI();
  const [journal, setJournal] = useState<OfflineJournal>();
  const [error, setError] = useState("");
  const [cleared, setCleared] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [reset, setReset] = useState(false);
  useEffect(() => {
    let active = true;
    void host
      .bootstrap()
      .then(async (boot) => {
        const next = offlineJournal(host, boot.hostId || "local");
        if (active) setJournal(next);
        await next.load();
        if (active) setError(next.historyError);
      })
      .catch((error) => {
        if (active) setError(errorMessage(error));
      });
    return () => {
      active = false;
    };
  }, [host]);
  return (
    <section className="settings-section">
      <h2>Offline history</h2>
      <p>
        Recent conversations and message drafts are encrypted on this device.
        History expires after seven days. Saved messages retry with their
        original IDs for up to 23 hours; uncertain delivery always needs your
        review. Permission answers require a live request.
      </p>
      {error && (
        <>
          <p role="status">{error}</p>
          <button
            disabled={!journal}
            onClick={() =>
              void journal!
                .load()
                .then(() =>
                  journal!.historyError ? journal!.retryHistory() : undefined,
                )
                .then(() => {
                  setError("");
                  window.dispatchEvent(
                    new CustomEvent("twr:offline-ready", {
                      detail: { hostId: journal!.hostId },
                    }),
                  );
                })
                .catch((error) => setError(errorMessage(error)))
            }
          >
            Retry encrypted storage
          </button>
          <button disabled={!journal} onClick={() => setReset(true)}>
            Reset offline storage…
          </button>
        </>
      )}
      <button disabled={!journal} onClick={() => setConfirm(true)}>
        Clear saved history…
      </button>
      {cleared && (
        <p role="status">
          Saved history cleared. New live activity can be cached again. Drafts
          and outbox messages are kept.
        </p>
      )}
      {reset && (
        <Dialog title="Reset offline storage?" onClose={() => setReset(false)}>
          <p>
            This removes this hub's saved history, drafts and outbox from this
            device. It does not recall messages already delivered to the hub.
          </p>
          <button onClick={() => setReset(false)}>Cancel</button>
          <button
            onClick={() =>
              void journal!
                .reset()
                .then(() => {
                  setError("");
                  setReset(false);
                  window.dispatchEvent(
                    new CustomEvent("twr:offline-reset", {
                      detail: { hostId: journal!.hostId },
                    }),
                  );
                })
                .catch((error) => setError(errorMessage(error)))
            }
          >
            Reset offline storage
          </button>
        </Dialog>
      )}
      {confirm && (
        <Dialog title="Clear saved history?" onClose={() => setConfirm(false)}>
          <p>
            This removes cached conversation history for this hub from this
            device. It keeps drafts, outbox messages, and everything on the hub.
          </p>
          <button onClick={() => setConfirm(false)}>Cancel</button>
          <button
            onClick={() =>
              void journal!
                .clearHistory()
                .then(() => {
                  setCleared(true);
                  setError("");
                  setConfirm(false);
                })
                .catch((e) => setError(errorMessage(e)))
            }
          >
            Clear history
          </button>
        </Dialog>
      )}
    </section>
  );
}
