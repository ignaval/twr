import { createHash, type Hash } from "node:crypto";
import {
  openSync,
  readSync,
  closeSync,
  statSync,
  readFileSync,
  writeFileSync,
  renameSync,
  existsSync,
  readdirSync,
  rmSync,
  promises as fs,
} from "node:fs";
import { join, dirname } from "node:path";
import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

/** Hash engines without retaining whole executable buffers in Electron's heap. */
export function fileDigest(path: string, aggregate?: Hash): string {
  const digest = createHash("sha256");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  const file = openSync(path, "r");
  try {
    for (;;) {
      const count = readSync(file, buffer, 0, buffer.length, null);
      if (!count) break;
      const bytes = buffer.subarray(0, count);
      digest.update(bytes);
      aggregate?.update(bytes);
    }
  } finally {
    closeSync(file);
  }
  return digest.digest("hex");
}

// ctime changes on writes even when a caller restores mtime. Reuse a digest only
// while the file's full identity and metadata remain unchanged.
export class EngineDigests {
  private entries: Record<string, { stamp: string; hash: string }> = {};
  constructor(private index: string) {
    try {
      if (statSync(index).size < 1024 * 1024) {
        const value = JSON.parse(readFileSync(index, "utf8"));
        if (value && typeof value === "object" && !Array.isArray(value))
          this.entries = value;
      }
    } catch {}
  }
  digest(path: string): string {
    const stat = statSync(path);
    const stamp = [
      stat.dev,
      stat.ino,
      stat.size,
      stat.mtimeMs,
      stat.ctimeMs,
      stat.mode,
    ].join(":");
    const saved = this.entries[path];
    if (saved?.stamp === stamp && /^[a-f0-9]{64}$/.test(saved.hash))
      return saved.hash;
    const hash = fileDigest(path);
    this.entries[path] = { stamp, hash };
    return hash;
  }
  save() {
    try {
      for (const path of Object.keys(this.entries))
        if (!existsSync(path)) delete this.entries[path];
      const scratch = this.index + `.${randomUUID()}.tmp`;
      writeFileSync(scratch, JSON.stringify(this.entries), { mode: 0o600 });
      renameSync(scratch, this.index);
    } catch {}
  }
}

export function pruneEngines(
  root: string,
  current: string,
  inUse: Set<string>,
  now = Date.now(),
) {
  const directories = readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && /^[a-f0-9]{64}$/.test(entry.name))
    .map((entry) => ({
      path: join(root, entry.name),
      modified: statSync(join(root, entry.name)).mtimeMs,
    }))
    .sort((a, b) => b.modified - a.modified);
  const keep = new Set([
    current,
    ...directories.slice(0, 2).map((entry) => entry.path),
    ...inUse,
  ]);
  for (const entry of directories)
    if (!keep.has(entry.path) && now - entry.modified > 7 * 86400 * 1000)
      rmSync(entry.path, { recursive: true, force: true });
}

export async function cleanupEngines(root: string, current: string) {
  const inUse = new Set<string>();
  if (process.platform === "darwin") {
    const { stdout } = await promisify(execFile)(
      "/usr/sbin/lsof",
      ["-a", "-u", String(process.getuid!()), "-d", "txt", "-F", "n"],
      { timeout: 15000, maxBuffer: 8 * 1024 * 1024, encoding: "utf8" },
    );
    for (const line of stdout.split("\n"))
      if (line.startsWith("n" + root + "/")) inUse.add(dirname(line.slice(1)));
  } else if (process.platform === "linux") {
    for (const pid of await fs.readdir("/proc")) {
      if (!/^\d+$/.test(pid)) continue;
      try {
        if ((await fs.stat(`/proc/${pid}`)).uid !== process.getuid!()) continue;
        const executable = await fs.readlink(`/proc/${pid}/exe`);
        if (executable.startsWith(root + "/")) inUse.add(dirname(executable));
      } catch (error: any) {
        if (error.code !== "ENOENT" && error.code !== "ESRCH") throw error;
      }
    }
  } else return;
  // If process enumeration fails, the caller retains every engine. Never remove
  // a running hub's executable merely because it came from an older app build.
  pruneEngines(root, current, inUse);
}
