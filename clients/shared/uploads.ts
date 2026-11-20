import { fileDigest, requestId } from "./remote-crypto";
import type { Json } from "./types";
import type { NamedFile } from "./attachmentText";

export const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;
export interface UploadResume {
  id: string;
  workspace: string;
  thread: string;
  sha256: string;
  offset: number;
}
export interface UploadProgress {
  phase: "hashing" | "uploading" | "committing";
  loaded: number;
  total: number;
  resume?: UploadResume;
}
export interface UploadOptions {
  workspace?: string;
  signal?: AbortSignal;
  resume?: UploadResume;
  onProgress?: (progress: UploadProgress) => void;
}
export type UploadFile = (
  file: File,
  thread: string,
  options?: UploadOptions,
) => Promise<{ path: string }>;
export type UploadRequest = (method: string, params: Json) => Promise<any>;
const paused = () => new DOMException("Upload paused", "AbortError");
function filename(file: File) {
  let name = file.name.normalize("NFC").replace(/[^\p{L}\p{N}_. -]/gu, "_");
  while (new TextEncoder().encode(name).length > 120)
    name = Array.from(name).slice(0, -1).join("");
  return !name || name === "." || name === ".." ? "attachment" : name;
}
async function base64(blob: Blob): Promise<string> {
  const bytes = new Uint8Array(await blob.arrayBuffer());
  let binary = "";
  for (let i = 0; i < bytes.length; i += 8192)
    binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return btoa(binary);
}
/** The same bounded, resumable transport is used by native desktop and mobile. */
export function createUploadFile(
  request: UploadRequest,
  workspace: () => string,
): UploadFile {
  return async (file, thread, options = {}) => {
    if (file.size > MAX_UPLOAD_BYTES)
      throw new Error("Files can be at most 25 MiB");
    const destination =
      options.workspace || options.resume?.workspace || workspace();
    const check = () => {
      if (options.signal?.aborted) throw paused();
      if (workspace() !== destination)
        throw new Error(
          "Workspace changed; return to the attachment's workspace to resume",
        );
    };
    check();
    options.onProgress?.({
      phase: "hashing",
      loaded: 0,
      total: file.size,
      resume: options.resume,
    });
    const sha256 = await fileDigest(await file.arrayBuffer());
    check();
    if (
      options.resume &&
      (options.resume.workspace !== destination ||
        options.resume.thread !== thread ||
        options.resume.sha256 !== sha256)
    )
      throw new Error(
        "Attachment destination or file changed; select the original file to resume",
      );
    const resume: UploadResume = {
      id: options.resume?.id || requestId(),
      workspace: destination,
      thread,
      sha256,
      offset: 0,
    };
    const params = () => ({ id: resume.id, thread, sha256 });
    const report = (phase: UploadProgress["phase"]) =>
      options.onProgress?.({
        phase,
        loaded: resume.offset,
        total: file.size,
        resume: { ...resume },
      });
    // Publish the ID before begin: a lost begin acknowledgement can be recovered.
    report("uploading");
    let status: any;
    if (options.resume) {
      try {
        status = await request("upload-status", params());
      } catch (error) {
        if (!/expired|not found/i.test(String(error))) throw error;
      }
    }
    check();
    if (!status)
      status = await request("upload-begin", {
        ...params(),
        name: filename(file),
        size: file.size,
      });
    check();
    const busyUntil = Date.now() + 30_000;
    while (status.busy) {
      check();
      if (Date.now() >= busyUntil)
        throw new Error(
          "An upload operation is still in progress. Resume to check its result.",
        );
      await new Promise((resolve) => setTimeout(resolve, 200));
      check();
      status = await request("upload-status", params());
    }
    const offset = status.received ?? 0;
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > file.size)
      throw new Error("Invalid upload offset");
    if (
      offset &&
      status.prefix_sha256 !==
        (await fileDigest(await file.slice(0, offset).arrayBuffer()))
    )
      throw new Error(
        "Received bytes do not match this file; cancel and upload again",
      );
    resume.offset = offset;
    report("uploading");
    check();
    if (typeof status.path === "string") return { path: status.path };
    const chunkSize = Math.min(65536, status.chunk_size || 65536);
    if (!Number.isSafeInteger(chunkSize) || chunkSize < 1)
      throw new Error("Invalid upload chunk size");
    while (resume.offset < file.size) {
      check();
      const end = Math.min(file.size, resume.offset + chunkSize);
      const data = await base64(file.slice(resume.offset, end));
      check();
      const ack = await request("upload-chunk", {
        ...params(),
        offset: resume.offset,
        data,
      });
      if (ack.received !== end)
        throw new Error(
          "Upload acknowledgement changed; resume to verify received bytes",
        );
      resume.offset = end;
      report("uploading");
    }
    check();
    report("committing");
    const committed = await request("upload-commit", params());
    if (typeof committed.path !== "string")
      throw new Error("Upload did not return a file path");
    return { path: committed.path };
  };
}
export type UploadCancelResult = void | {
  committed?: boolean;
  path?: string;
  cancelled?: boolean;
  busy?: boolean;
};
export function createCancelUpload(
  request: UploadRequest,
  workspace?: () => string,
) {
  return async (resume: UploadResume): Promise<UploadCancelResult> => {
    if (workspace && resume.workspace !== workspace())
      throw new Error(
        "Workspace changed; return to the attachment's workspace to cancel",
      );
    return await request("upload-cancel", {
      id: resume.id,
      thread: resume.thread,
      sha256: resume.sha256,
    });
  };
}

export interface Attachment {
  id: string;
  file: File;
  thread: string;
  workspace: string;
  phase:
    | "queued"
    | UploadProgress["phase"]
    | "paused"
    | "error"
    | "ready"
    | "cancelling";
  loaded: number;
  error?: string;
  path?: string;
  resume?: UploadResume;
  image?: string;
  text?: string;
  textTruncated?: boolean;
  /** Put back from a message taken back to the draft: the file is not in memory, only
   * its name and its uploaded copy's path. */
  restored?: boolean;
}
/** The most attachments one conversation holds, and the most uploads unfinished at once. */
export const MAX_ATTACHMENTS = 8;
/** Whether an attachment is still on its way: a message waits for it (Send is disabled). */
export function unfinished(item: Attachment) {
  return item.phase !== "ready";
}
interface UploadHost {
  uploadFile?: UploadFile;
  cancelUpload?(resume: UploadResume): Promise<UploadCancelResult>;
}
type Job = {
  item: Attachment;
  abort?: AbortController;
  running?: Promise<void>;
  cancelled?: boolean;
};
/**
 * The attachments of every conversation: each belongs to the thread (and workspace) it was
 * picked in, and stays there, uploaded or not, until a message to that conversation takes
 * it ([`UploadController.take`]) or you remove it. Queue state is independent of a mounted
 * composer: navigating threads preserves progress.
 */
export class UploadController {
  private jobs = new Map<string, Job>();
  private listeners = new Set<() => void>();
  private snapshot: Attachment[] = [];
  private disposed = false;
  private workspace?: string;
  /** Image previews of sent attachments, by uploaded path, for the sent message's chips:
   * the newest few, released as newer ones arrive and on dispose. */
  private sentImages = new Map<string, string>();
  setWorkspace(workspace: string) {
    this.workspace = workspace;
    for (const job of this.jobs.values())
      if (job.running && job.item.workspace !== workspace) job.abort?.abort();
    this.pump();
  }
  constructor(
    private host: UploadHost,
    /** Told when an upload finishes. It no longer writes anything into a draft. */
    private uploaded: (attachment: Attachment) => void = () => {},
  ) {}
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  getSnapshot = () => this.snapshot;
  private emit() {
    this.snapshot = [...this.jobs.values()].map((job) => ({ ...job.item }));
    this.listeners.forEach((listener) => listener());
  }
  add(files: File[], thread: string, workspace: string) {
    if (!this.host.uploadFile)
      throw new Error("File uploads are unavailable on this connection");
    // A ready attachment waits for its message: it is never evicted to make room.
    if (
      this.of(thread, workspace).length + files.length > MAX_ATTACHMENTS ||
      [...this.jobs.values()].filter((job) => unfinished(job.item)).length +
        files.length >
        MAX_ATTACHMENTS
    )
      throw new Error(
        `Attach at most ${MAX_ATTACHMENTS} files at a time: send or remove some first`,
      );
    if (files.some((file) => file.size > MAX_UPLOAD_BYTES))
      throw new Error("Files can be at most 25 MiB");
    for (const file of files) {
      const item: Attachment = {
        id: requestId(),
        file,
        thread,
        workspace,
        phase: "queued",
        loaded: 0,
      };
      const job: Job = { item };
      this.jobs.set(item.id, job);
      if (
        /^image\/(png|jpeg|gif|webp|avif)$/.test(file.type) &&
        file.size <= 10 * 1024 * 1024
      )
        item.image = URL.createObjectURL(file);
      if (
        /^text\//.test(file.type) ||
        /\.(txt|md|json|csv|log|ya?ml|toml|[jt]sx?|rs|py|css|html)$/i.test(
          file.name,
        )
      ) {
        void file
          .slice(0, 65536)
          .text()
          .then((text) => {
            if (this.jobs.has(item.id)) {
              item.text = text;
              item.textTruncated = file.size > 65536;
              this.emit();
            }
          })
          .catch(() => {});
      }
    }
    this.emit();
    this.pump();
  }
  private pump() {
    if (this.disposed) return;
    let slots = 2 - [...this.jobs.values()].filter((job) => job.running).length;
    for (const job of this.jobs.values()) {
      if (slots <= 0) break;
      if (job.item.phase !== "queued" || job.running) continue;
      if (this.workspace !== undefined && job.item.workspace !== this.workspace)
        continue;
      slots--;
      job.abort = new AbortController();
      job.item.phase = "hashing";
      job.running = this.run(job).finally(() => {
        job.running = undefined;
        this.emit();
        this.pump();
      });
    }
    this.emit();
  }
  /** One conversation's attachments, in the order they were picked. */
  of(thread: string, workspace: string): Attachment[] {
    return this.snapshot.filter(
      (item) => item.thread === thread && item.workspace === workspace,
    );
  }
  /** One conversation's uploaded attachments: what its next message carries. */
  ready(thread: string, workspace: string): Attachment[] {
    return this.of(thread, workspace).filter(
      (item) => item.phase === "ready" && item.path,
    );
  }
  /**
   * Takes these attachments off their conversation: the message that names them was
   * accepted (sent, queued, or saved on this device). An image's preview is kept for the
   * sent message's chip.
   */
  take(ids: readonly string[]) {
    for (const id of ids) {
      const job = this.jobs.get(id);
      if (!job) continue;
      this.jobs.delete(id);
      if (job.item.image && job.item.path)
        this.keepImage(job.item.path, job.item.image);
      else if (job.item.image) URL.revokeObjectURL(job.item.image);
    }
    this.emit();
  }
  /**
   * Puts files a message named back on a conversation, uploaded (a message taken back to
   * the draft): its next message names them again. A file already there is not added twice.
   */
  restore(files: readonly NamedFile[], thread: string, workspace: string) {
    const known = new Set(this.of(thread, workspace).map((item) => item.path));
    for (const named of files) {
      if (known.has(named.path)) continue;
      known.add(named.path);
      const item: Attachment = {
        id: requestId(),
        file: new File([], named.name),
        thread,
        workspace,
        phase: "ready",
        loaded: 0,
        path: named.path,
        restored: true,
      };
      const image = this.sentImages.get(named.path);
      if (image) {
        // The conversation's chip owns the preview again.
        this.sentImages.delete(named.path);
        item.image = image;
      }
      this.jobs.set(item.id, { item });
    }
    this.emit();
  }
  /** The image preview this client still holds for a sent attachment's path, if any. */
  sentImage = (path: string): string | undefined => this.sentImages.get(path);
  private keepImage(path: string, url: string) {
    const old = this.sentImages.get(path);
    if (old && old !== url) URL.revokeObjectURL(old);
    this.sentImages.delete(path);
    this.sentImages.set(path, url);
    while (this.sentImages.size > 16) {
      const [oldest, image] = this.sentImages.entries().next().value!;
      URL.revokeObjectURL(image);
      this.sentImages.delete(oldest);
    }
  }
  private async run(job: Job) {
    try {
      const out = await this.host.uploadFile!(job.item.file, job.item.thread, {
        workspace: job.item.workspace,
        signal: job.abort!.signal,
        resume: job.item.resume,
        onProgress: (progress) => {
          Object.assign(job.item, progress);
          this.emit();
        },
      });
      if (!job.cancelled && !this.disposed) {
        job.item.path = out.path;
        job.item.phase = "ready";
        job.item.loaded = job.item.file.size;
        this.uploaded({ ...job.item });
      }
    } catch (error) {
      if (!job.cancelled) {
        job.item.phase = job.abort?.signal.aborted ? "paused" : "error";
        job.item.error = String(error);
      }
    }
  }
  pause(id: string) {
    const job = this.jobs.get(id);
    if (job?.running) job.abort?.abort();
    else if (job?.item.phase === "queued") {
      job.item.phase = "paused";
      this.emit();
    }
  }
  resume(id: string) {
    const job = this.jobs.get(id);
    if (!job || job.running || !["paused", "error"].includes(job.item.phase))
      return;
    job.item.phase = "queued";
    job.item.error = undefined;
    this.emit();
    this.pump();
  }
  async remove(id: string) {
    const job = this.jobs.get(id);
    if (!job) return;
    job.cancelled = true;
    job.abort?.abort();
    job.item.phase = "cancelling";
    this.emit();
    await job.running;
    try {
      if (job.item.resume && !job.item.path) {
        const result = await this.host.cancelUpload?.(job.item.resume);
        if (result?.busy)
          throw new Error(
            "An upload operation is still in progress. Resume to check its result, then cancel again if needed.",
          );
        if (result?.committed && result.path) {
          job.cancelled = false;
          job.item.phase = "ready";
          job.item.path = result.path;
          job.item.error =
            "Already uploaded, so it stays attached. Remove it again to leave it out of your message.";
          this.emit();
          return;
        }
      }
    } catch (error) {
      job.cancelled = false;
      job.item.phase = "error";
      job.item.error = `Cancellation failed: ${String(error)}`;
      this.emit();
      return;
    }
    if (job.item.image) URL.revokeObjectURL(job.item.image);
    this.jobs.delete(id);
    this.emit();
  }
  dispose() {
    this.disposed = true;
    for (const job of this.jobs.values()) {
      job.abort?.abort();
      if (job.item.image) URL.revokeObjectURL(job.item.image);
    }
    for (const image of this.sentImages.values()) URL.revokeObjectURL(image);
    this.sentImages.clear();
    this.listeners.clear();
  }
}
