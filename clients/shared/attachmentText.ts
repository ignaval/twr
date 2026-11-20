// How a message names its attachments: one `Uploaded file: <name>` line and the uploaded
// copy's path on the next, per file, after what you typed. The composer adds them as it
// sends (never to the draft), so the orchestrator and workers get the paths; a sent message
// shows them as chips with the name only. Only a path where twr puts an upload counts
// (`.twr-uploads/<directory>/<name>`), never inside a fenced code block, so text that only
// looks like a pair stays text.

/** A file a message names: its name as picked, and where the hub put the copy. */
export interface NamedFile {
  name: string;
  path: string;
}

const PREFIX = "Uploaded file: ";

/**
 * A path line as uploads write it: absolute on the box (POSIX or a Windows drive), on one
 * line, ending in the upload's `.twr-uploads/<directory>/<name>`. Taken exactly as it is,
 * trailing spaces too: an older app stored a name ending in one (`notes.txt `), and that is
 * its file's path.
 */
function isPath(line: string) {
  return /^(?:\/|[A-Za-z]:[\\/])(?:[^\p{Cc}]*[\\/])?\.twr-uploads[\\/][^\\/\p{Cc}]+[\\/][^\\/\p{Cc}]+$/u.test(
    line,
  );
}

/** A line that opens a fenced code block (CommonMark: up to three spaces, then three or
 * more backticks or tildes), and the fence that closes it. */
function fenceOf(line: string) {
  return /^ {0,3}(`{3,}|~{3,})/.exec(line)?.[1];
}
function closes(line: string, fence: string) {
  const at = line.match(/^ {0,3}(`{3,}|~{3,})\s*$/)?.[1];
  return !!at && at[0] === fence[0] && at.length >= fence.length;
}

/**
 * A file's name as its `Uploaded file:` line holds it: on one line. A name may hold line
 * breaks (`\n`, `\r`, U+2028 and the like) or other control characters, which would split
 * the pair and leave the path as message text; each run of them becomes one space.
 */
export function lineName(name: string) {
  const flat = name.replace(/[\p{Cc}\u2028\u2029]+/gu, " ").trim();
  return flat || "attachment";
}

/** The lines naming `files`, one pair per file. */
export function attachmentLines(files: readonly NamedFile[]) {
  return files
    .map((file) => `${PREFIX}${lineName(file.name)}\n${file.path}`)
    .join("\n");
}

/** `text` as sent with `files`: the text, then each file's pair on its own lines. */
export function withAttachments(text: string, files: readonly NamedFile[]) {
  if (!files.length) return text;
  const body = text.replace(/\s+$/, "");
  return (body ? body + "\n" : "") + attachmentLines(files);
}

/**
 * The files a message names, and its text without their lines. Only the exact two-line
 * form counts: a whole `Uploaded file: <name>` line followed by a whole line that is an
 * upload's path (`isPath`), outside fenced code blocks. The pairs that end a message count
 * even in a fence left open: that is where the composer writes them. Anything else stays
 * text, so ordinary messages are never rewritten.
 */
export function splitAttachments(text: string): {
  body: string;
  files: NamedFile[];
} {
  if (!text.includes(PREFIX)) return { body: text, files: [] };
  const lines = text.split("\n");
  /** The name of the pair that starts at line `i`, if one does. */
  const pairAt = (i: number) => {
    const line = lines[i];
    const next = lines[i + 1];
    const name = line.startsWith(PREFIX) ? line.slice(PREFIX.length) : "";
    return name.trim() && next !== undefined && isPath(next) ? name : undefined;
  };
  // The pairs that end the message: from `tail` to `end` (blank lines after them aside).
  let end = lines.length;
  while (end > 0 && !lines[end - 1].trim()) end--;
  let tail = end;
  while (tail >= 2 && pairAt(tail - 2) !== undefined) tail -= 2;
  const kept: string[] = [];
  const files: NamedFile[] = [];
  let fence: string | undefined;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const name = i >= tail && i < end ? pairAt(i) : undefined;
    if (name !== undefined) {
      files.push({ name, path: lines[i + 1] });
      i++;
    } else if (fence) {
      if (closes(line, fence)) fence = undefined;
      kept.push(line);
    } else if ((fence = fenceOf(line))) kept.push(line);
    else {
      const name = pairAt(i);
      if (name !== undefined) {
        files.push({ name, path: lines[i + 1] });
        i++;
      } else kept.push(line);
    }
  }
  if (!files.length) return { body: text, files };
  return { body: kept.join("\n").replace(/\s+$/, ""), files };
}

/**
 * `text` without the whitespace around it, except for an upload's path that ends it (the
 * pairs come last): that line is kept exactly, a trailing space too, as the hub keeps it
 * (`twr_core::upload::trim_message`). Only the line breaks after it go.
 */
export function trimMessage(text: string) {
  const start = text.replace(/^\s+/, "");
  const lines = start.replace(/[\r\n]+$/, "").split("\n");
  const last = lines.at(-1) ?? "";
  const named = lines.at(-2)?.startsWith(PREFIX) ?? false;
  return named && isPath(last) ? lines.join("\n") : start.replace(/\s+$/, "");
}

/** `text` to read where no chip can show: its files by name on a last line
 * (`Attached: a.png, notes.txt`), never their paths. */
export function namedText(text: string) {
  const { body, files } = splitAttachments(text);
  if (!files.length) return text;
  const names = `Attached: ${files.map((file) => lineName(file.name)).join(", ")}`;
  return body ? `${body}\n\n${names}` : names;
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
