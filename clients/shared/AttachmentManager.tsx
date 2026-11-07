import { useSyncExternalStore } from "react";
import type { UploadController } from "./uploads";
import "./attachments.css";

export function AttachmentManager({
  controller,
  thread,
  workspace,
}: {
  controller: UploadController;
  thread: string;
  workspace: string;
}) {
  const items = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
  ).filter((item) => item.thread === thread && item.workspace === workspace);
  if (!items.length) return null;
  return (
    <div className="attachment-manager" aria-label="Attachments">
      {items.map((item) => (
        <article className="attachment-item" key={item.id}>
          {item.image && (
            <img
              className="attachment-preview"
              src={item.image}
              alt={item.file.name}
            />
          )}
          <div className="attachment-details">
            <strong>{item.file.name}</strong>
            <span>
              {(item.file.size / 1024).toFixed(1)} KiB ·{" "}
              {item.phase === "ready" ? "Ready to send" : item.phase}
            </span>
            {!["ready", "error", "paused"].includes(item.phase) && (
              <progress
                aria-label={`Uploading ${item.file.name}`}
                value={item.loaded}
                max={item.file.size || 1}
              />
            )}
            {item.text !== undefined && (
              <details>
                <summary>
                  Preview text{item.textTruncated ? " (first 64 KiB)" : ""}
                </summary>
                <pre>{item.text}</pre>
              </details>
            )}
            {item.error && <p role="status">{item.error}</p>}
          </div>
          <div className="attachment-actions">
            {["hashing", "uploading", "queued"].includes(item.phase) && (
              <button type="button" onClick={() => controller.pause(item.id)}>
                Pause
              </button>
            )}
            {["paused", "error"].includes(item.phase) && (
              <button type="button" onClick={() => controller.resume(item.id)}>
                Resume
              </button>
            )}
            <button
              type="button"
              disabled={item.phase === "cancelling"}
              onClick={() => void controller.remove(item.id)}
            >
              {item.phase === "ready" ? "Dismiss" : "Cancel"}
            </button>
          </div>
        </article>
      ))}
    </div>
  );
}
