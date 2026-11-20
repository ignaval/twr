// The files attached to the message being written, inside the message box above its text:
// an image as a small tile, any other file as a chip. Neither shows where the file went
// (its path): the composer adds the paths to the message as it sends it (attachmentText.ts).
// A sent message shows the files it named as chips too (`SentAttachments`).
import {
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
} from "react";
import {
  File as FileIcon,
  FileCode,
  FileText,
  Image as ImageIcon,
  Paperclip,
  Pause,
  Play,
  RotateCw,
  X,
} from "lucide-react";
import { announce } from "./a11y";
import {
  fileKind,
  fileSize,
  splitAttachments,
  type NamedFile,
} from "./attachmentText";
import { AppLayer } from "./LoopViews";
import { MessageBody } from "./MessageBody";
import { Dialog } from "./ui";
import type { Attachment, UploadController } from "./uploads";
import "./attachments.css";

const kindIcon = {
  image: ImageIcon,
  text: FileText,
  code: FileCode,
  file: FileIcon,
};
function KindIcon({ name, size = 14 }: { name: string; size?: number }) {
  const Icon = kindIcon[fileKind(name)];
  return <Icon size={size} aria-hidden="true" />;
}

const uploading = (item: Attachment) =>
  ["queued", "hashing", "uploading", "committing"].includes(item.phase);
/** How far an upload is, in whole percent. */
export function uploadPercent(item: Attachment) {
  if (item.phase === "ready") return 100;
  const total = item.file.size;
  if (!total) return item.phase === "committing" ? 99 : 0;
  return Math.min(99, Math.floor((item.loaded / total) * 100));
}
/** What an attachment is doing, in a few words. */
export function attachmentStatus(item: Attachment) {
  switch (item.phase) {
    case "ready":
      return item.restored ? "Attached" : `${fileSize(item.file.size)} · ready`;
    case "queued":
      return "Waiting to upload";
    case "hashing":
      return "Preparing";
    case "uploading":
    case "committing":
      return `Uploading ${uploadPercent(item)}%`;
    case "paused":
      return "Paused";
    case "cancelling":
      return "Removing";
    case "error":
      return "Upload failed";
  }
}

/** Why Send waits while files are attached that are not uploaded yet. */
export function attachmentWait(items: readonly Attachment[]) {
  const stuck = items.find((item) => ["paused", "error"].includes(item.phase));
  if (stuck)
    return `Resume or remove ${stuck.file.name} to send: a message waits for its files`;
  return "Send waits until your files finish uploading";
}

/** The strip in the message box. Renders nothing while the conversation has no files. */
export function ComposerAttachments({
  controller,
  conversation,
  workspace,
}: {
  controller: UploadController;
  /** `orchestrator`, or a thread's id (`Attachment.conversation`). */
  conversation?: string;
  workspace: string;
}) {
  const all = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
    controller.getSnapshot,
  );
  const items = all.filter(
    (item) =>
      item.conversation === conversation && item.workspace === workspace,
  );
  const [shown, setShown] = useState<string>();
  const phases = useRef(new Map<string, string>());
  useEffect(() => {
    // Announce completion and failure once; progress itself stays silent.
    const seen = phases.current;
    for (const item of all) {
      const previous = seen.get(item.id);
      if (previous !== undefined && previous !== item.phase) {
        if (item.phase === "ready")
          announce(
            `${item.file.name} uploaded. It goes with your next message`,
          );
        else if (item.phase === "error")
          announce(
            `Upload failed for ${item.file.name}${item.error ? ": " + item.error : ""}`,
          );
      }
      seen.set(item.id, item.phase);
    }
  });
  const open = items.find((item) => item.id === shown);
  if (!items.length) return null;
  return (
    <div className="composer-attachments">
      <ul aria-label="Attached files">
        {items.map((item) =>
          item.image ? (
            <AttachmentTile
              key={item.id}
              item={item}
              controller={controller}
              onOpen={() => setShown(item.id)}
            />
          ) : (
            <AttachmentChip
              key={item.id}
              item={item}
              controller={controller}
              onOpen={() => setShown(item.id)}
            />
          ),
        )}
      </ul>
      {items.some((item) => item.phase !== "ready") && (
        <p className="attachment-wait" id="twr-attachment-wait">
          {attachmentWait(items)}
        </p>
      )}
      {open && (
        <AppLayer>
          <AttachmentDialog
            item={open}
            controller={controller}
            onClose={() => setShown(undefined)}
          />
        </AppLayer>
      )}
    </div>
  );
}

/** The ✕ on a tile or chip: removes a file, or cancels its upload. */
function RemoveButton({
  item,
  controller,
}: {
  item: Attachment;
  controller: UploadController;
}) {
  const label =
    item.phase === "ready"
      ? `Remove ${item.file.name}`
      : `Cancel upload of ${item.file.name}`;
  return (
    <button
      type="button"
      className="attachment-remove"
      aria-label={label}
      title={item.phase === "ready" ? "Remove" : "Cancel upload"}
      disabled={item.phase === "cancelling"}
      onClick={() => void controller.remove(item.id)}
    >
      <span aria-hidden="true">
        <X size={11} strokeWidth={2.5} />
      </span>
    </button>
  );
}

/**
 * What a tile's or chip's own button does: retry a failed upload, resume a paused one, and
 * otherwise open the file's preview and its controls. An error is its description.
 */
function mainAction(
  item: Attachment,
  controller: UploadController,
  onOpen: () => void,
) {
  if (item.phase === "error")
    return {
      label: `Retry uploading ${item.file.name}`,
      title: item.error ? `Upload failed: ${item.error}. Retry` : "Retry",
      run: () => controller.resume(item.id),
    };
  if (item.phase === "paused")
    return {
      label: `Resume uploading ${item.file.name}`,
      title: "Resume upload",
      run: () => controller.resume(item.id),
    };
  return {
    label: `Show ${item.file.name}`,
    title: item.file.name,
    run: onOpen,
  };
}

function AttachmentTile({
  item,
  controller,
  onOpen,
}: {
  item: Attachment;
  controller: UploadController;
  onOpen: () => void;
}) {
  const errorId = useId();
  const action = mainAction(item, controller, onOpen);
  return (
    <li className="attachment-tile" data-phase={item.phase}>
      <button
        type="button"
        className="attachment-open"
        aria-label={action.label}
        title={action.title}
        aria-describedby={item.error ? errorId : undefined}
        onClick={action.run}
      >
        <img src={item.image} alt="" draggable={false} />
        {item.phase === "error" ? (
          <span className="attachment-state" aria-hidden="true">
            <RotateCw size={16} />
          </span>
        ) : item.phase === "paused" ? (
          <span className="attachment-state" aria-hidden="true">
            <Play size={16} />
          </span>
        ) : null}
        {uploading(item) && (
          <span className="attachment-percent" aria-hidden="true">
            {uploadPercent(item)}%
          </span>
        )}
      </button>
      <Progress item={item} />
      {item.error && (
        <span id={errorId} className="visually-hidden">
          {item.error}
        </span>
      )}
      <RemoveButton item={item} controller={controller} />
    </li>
  );
}

function AttachmentChip({
  item,
  controller,
  onOpen,
}: {
  item: Attachment;
  controller: UploadController;
  onOpen: () => void;
}) {
  const errorId = useId();
  const action = mainAction(item, controller, onOpen);
  return (
    <li className="attachment-chip" data-phase={item.phase}>
      <button
        type="button"
        className="attachment-open"
        aria-label={action.label}
        title={action.title}
        aria-describedby={item.error ? errorId : undefined}
        onClick={action.run}
      >
        {item.phase === "error" ? (
          <RotateCw size={14} aria-hidden="true" />
        ) : item.phase === "paused" ? (
          <Play size={14} aria-hidden="true" />
        ) : (
          <KindIcon name={item.file.name} />
        )}
        <span className="attachment-name">{item.file.name}</span>
        <span className="attachment-meta" aria-hidden="true">
          {item.phase === "ready"
            ? item.restored
              ? ""
              : fileSize(item.file.size)
            : uploading(item)
              ? `${uploadPercent(item)}%`
              : attachmentStatus(item)}
        </span>
      </button>
      <Progress item={item} />
      {item.error && (
        <span id={errorId} className="visually-hidden">
          {item.error}
        </span>
      )}
      <RemoveButton item={item} controller={controller} />
    </li>
  );
}

/** An upload's progress, for assistive technology (drawn as the tile's or chip's fill). */
function Progress({ item }: { item: Attachment }) {
  if (!uploading(item)) return null;
  const percent = uploadPercent(item);
  return (
    <span
      className="attachment-bar"
      role="progressbar"
      aria-label={`Uploading ${item.file.name}`}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
      style={{ "--progress": percent / 100 } as CSSProperties}
    />
  );
}

/** A file's larger preview (an image, or a text file's first 64 KiB) and its controls. */
function AttachmentDialog({
  item,
  controller,
  onClose,
}: {
  item: Attachment;
  controller: UploadController;
  onClose: () => void;
}) {
  return (
    <Dialog
      title={item.file.name}
      subtitle={attachmentStatus(item)}
      onClose={onClose}
      wide={Boolean(item.image || item.text !== undefined)}
    >
      <div className="attachment-dialog">
        {item.image ? (
          <img src={item.image} alt={item.file.name} />
        ) : item.text !== undefined ? (
          <>
            {item.textTruncated && (
              <p className="muted">The first 64 KiB of the file.</p>
            )}
            <pre tabIndex={0} aria-label={`Text of ${item.file.name}`}>
              {item.text}
            </pre>
          </>
        ) : (
          <p className="muted">
            {item.restored
              ? "This file was uploaded with an earlier message. It goes with your next one."
              : "No preview for this kind of file. It goes with your next message."}
          </p>
        )}
        {item.error && <p className="attachment-error">{item.error}</p>}
      </div>
      <footer className="dialog-footer">
        <span />
        <div>
          {uploading(item) && (
            <button type="button" onClick={() => controller.pause(item.id)}>
              <Pause size={14} aria-hidden="true" /> Pause
            </button>
          )}
          {["paused", "error"].includes(item.phase) && (
            <button type="button" onClick={() => controller.resume(item.id)}>
              <Play size={14} aria-hidden="true" />{" "}
              {item.phase === "error" ? "Retry" : "Resume"}
            </button>
          )}
          <button
            type="button"
            className="danger-quiet"
            disabled={item.phase === "cancelling"}
            onClick={() => {
              onClose();
              void controller.remove(item.id);
            }}
          >
            {item.phase === "ready" ? "Remove" : "Cancel upload"}
          </button>
          <button type="button" className="primary" onClick={onClose}>
            Done
          </button>
        </div>
      </footer>
    </Dialog>
  );
}

/**
 * The files a sent message named, as chips with the name only (an image with its preview
 * when this client still holds it; nothing is fetched).
 */
export function SentAttachments({
  files,
  image,
}: {
  files: readonly NamedFile[];
  /** The preview this client still holds for an uploaded path. */
  image?: (path: string) => string | undefined;
}) {
  if (!files.length) return null;
  return (
    <ul className="sent-attachments" aria-label="Attached files">
      {files.map((file, i) => {
        const src = image?.(file.path);
        return (
          <li key={file.path + i} title={file.name}>
            {src ? (
              <img src={src} alt="" draggable={false} />
            ) : (
              <KindIcon name={file.name} />
            )}
            <span>{file.name}</span>
          </li>
        );
      })}
    </ul>
  );
}

/** A queued message on one line: its text, then the names of the files it carries. */
export function QueuedText({ text }: { text: string }) {
  const { body, files } = splitAttachments(text);
  return (
    <span className="queued-text">
      {body}
      {files.length > 0 && (
        <span className="queued-files">
          <Paperclip size={12} aria-hidden="true" />
          <span className="visually-hidden">Attached: </span>
          {files.map((file) => file.name).join(", ")}
        </span>
      )}
    </span>
  );
}

/** Your sent message: its text, and the files it named as chips (the exact
 * `Uploaded file:` pairs only). */
export function SentMessage({
  text,
  sentImage,
  expanded,
  onExpandedChange,
}: {
  text: string;
  sentImage?: (path: string) => string | undefined;
  expanded?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
}) {
  const { body, files } = useMemo(() => splitAttachments(text), [text]);
  return (
    <>
      {(body || !files.length) && (
        <MessageBody
          text={body}
          clamp
          expanded={expanded}
          onExpandedChange={onExpandedChange}
        />
      )}
      <SentAttachments files={files} image={sentImage} />
    </>
  );
}
