// How a message names its attachments: one `Uploaded file: <name>` line and the uploaded
// copy's path on the next, per file, after what you typed. The composer adds them as it
// sends (never to the draft), so the orchestrator and workers get the paths; a sent message
// shows them as chips with the name only.

/** A file a message names: its name as picked, and where the hub put the copy. */
export interface NamedFile {
  name: string;
  path: string;
}

const PREFIX = "Uploaded file: ";

/** A path line as uploads write it: absolute on the box (POSIX or a Windows drive). */
function isPath(line: string) {
  return /^(?:\/|[A-Za-z]:[\\/])\S/.test(line) && line.trim() === line;
}

/** The lines naming `files`, one pair per file. */
export function attachmentLines(files: readonly NamedFile[]) {
  return files.map((file) => `${PREFIX}${file.name}\n${file.path}`).join("\n");
}

/** `text` as sent with `files`: the text, then each file's pair on its own lines. */
export function withAttachments(text: string, files: readonly NamedFile[]) {
  if (!files.length) return text;
  const body = text.replace(/\s+$/, "");
  return (body ? body + "\n" : "") + attachmentLines(files);
}

/**
 * The files a message names, and its text without their lines. Only the exact two-line
 * form counts: a whole `Uploaded file: <name>` line followed by a whole absolute-path line.
 * Anything else stays text, so ordinary messages are never rewritten.
 */
export function splitAttachments(text: string): {
  body: string;
  files: NamedFile[];
} {
  if (!text.includes(PREFIX)) return { body: text, files: [] };
  const lines = text.split("\n");
  const kept: string[] = [];
  const files: NamedFile[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const next = lines[i + 1];
    const name = line.startsWith(PREFIX) ? line.slice(PREFIX.length) : "";
    if (name.trim() && next !== undefined && isPath(next)) {
      files.push({ name, path: next });
      i++;
    } else kept.push(line);
  }
  if (!files.length) return { body: text, files };
  return { body: kept.join("\n").replace(/\s+$/, ""), files };
}

/** A file's kind from its name, for its chip's icon. */
export function fileKind(name: string): "image" | "text" | "code" | "file" {
  if (/\.(png|jpe?g|gif|webp|avif|svg|heic|bmp)$/i.test(name)) return "image";
  if (/\.([jt]sx?|rs|py|go|c|h|cpp|java|kt|swift|rb|sh|css|html)$/i.test(name))
    return "code";
  if (/\.(txt|md|json|csv|log|ya?ml|toml)$/i.test(name)) return "text";
  return "file";
}

/** A size in the largest fitting unit: `512 B`, `4.2 KiB`, `3.1 MiB`. */
export function fileSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}
