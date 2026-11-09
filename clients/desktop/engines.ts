import { createHash, type Hash } from "node:crypto";
import {
  createReadStream,
  type Stats,
  openSync,
  readSync,
  closeSync,
  statSync,
  realpathSync,
  readFileSync,
  writeFileSync,
  renameSync,
  existsSync,
  readdirSync,
  rmSync,
  promises as fs,
} from "node:fs";
import { join, dirname } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

export const ENGINE_BINARIES = [
  "twr",
  "twr-desktop-bridge",
  "twr-mobile-gateway",
] as const;

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

/** The same digest as fileDigest, read in the background so the window keeps painting. */
export async function streamDigest(path: string): Promise<string> {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(path, {
    highWaterMark: 1024 * 1024,
  }))
    digest.update(chunk as Buffer);
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
  async digest(path: string): Promise<string> {
    const stamp = fileStamp(await fs.stat(path));
    const saved = this.entries[path];
    if (saved?.stamp === stamp && /^[a-f0-9]{64}$/.test(saved.hash))
      return saved.hash;
    const hash = await streamDigest(path);
    // A file that changed while it was read gets no cached digest.
    if (fileStamp(await fs.stat(path)) === stamp)
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

function fileStamp(stat: Stats) {
  return [
    stat.dev,
    stat.ino,
    stat.size,
    stat.mtimeMs,
    stat.ctimeMs,
    stat.mode,
  ].join(":");
}

/** The manifest scripts/build-rust.mjs writes beside a build's engines. */
export const SHIPPED_DIGESTS = "digests.json";

/** The digests a packaged build ships beside its engines, if present and well formed. */
export function readShippedDigests(
  directory: string,
): Map<string, string> | undefined {
  try {
    const path = join(directory, SHIPPED_DIGESTS);
    if (statSync(path).size > 64 * 1024) return undefined;
    const value = JSON.parse(readFileSync(path, "utf8"));
    const digests = new Map<string, string>();
    for (const name of ENGINE_BINARIES) {
      const digest = value?.[name];
      if (typeof digest !== "string" || !/^[a-f0-9]{64}$/.test(digest))
        return undefined;
      digests.set(name, digest);
    }
    return digests;
  } catch {
    return undefined;
  }
}

class EngineMismatch extends Error {}

/**
 * Copies the engines in `source` into `root/<digest>`, a directory named by the
 * SHA-256 of its contents, and returns that directory. A packaged build passes
 * `trustShipped` to take the source digests from its own manifest: an AppImage
 * mounts at a new path on every launch, so a path-keyed cache would never hit.
 * Every copy is verified against its expected digest before it is moved into
 * place, and a manifest that does not match its engines is ignored.
 */
export async function stageEngines(
  source: string,
  root: string,
  trustShipped = false,
): Promise<string> {
  await fs.mkdir(root, { recursive: true, mode: 0o700 });
  const cache = new EngineDigests(join(root, "digests.json"));
  const shipped = trustShipped ? readShippedDigests(source) : undefined;
  try {
    return await stage(source, root, cache, shipped);
  } catch (error) {
    if (!(error instanceof EngineMismatch) || !shipped) throw error;
    return await stage(source, root, cache, undefined);
  } finally {
    cache.save();
  }
}

async function stage(
  source: string,
  root: string,
  cache: EngineDigests,
  shipped: Map<string, string> | undefined,
) {
  const expected = new Map<string, string>();
  for (const name of ENGINE_BINARIES)
    expected.set(
      name,
      shipped?.get(name) ?? (await cache.digest(join(source, name))),
    );
  const digest = createHash("sha256")
    .update(JSON.stringify([...expected]))
    .digest("hex");
  const stable = join(root, digest);
  await fs.mkdir(stable, { recursive: true, mode: 0o700 });
  for (const name of ENGINE_BINARIES) {
    const target = join(stable, name);
    if (
      existsSync(target) &&
      (await cache.digest(target)) === expected.get(name)
    )
      continue;
    const scratch = join(stable, `.${name}-${randomUUID()}`);
    try {
      await fs.copyFile(join(source, name), scratch);
      await fs.chmod(scratch, 0o755);
      if ((await streamDigest(scratch)) !== expected.get(name))
        throw new EngineMismatch(`${name} does not match its expected digest`);
      await fs.rename(scratch, target);
    } finally {
      await fs.rm(scratch, { force: true });
    }
    await cache.digest(target);
  }
  return stable;
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

export async function referencedEngines(
  root: string,
  serviceDirectories: string[],
): Promise<Set<string>> {
  const retained = new Set<string>();
  for (const directory of serviceDirectories) {
    let entries: string[];
    try {
      entries = await fs.readdir(directory);
    } catch (error: any) {
      if (error.code === "ENOENT") continue;
      throw error;
    }
    for (const name of entries) {
      if (
        !(name.startsWith("tools.twr.") && name.endsWith(".plist")) &&
        !(name.startsWith("twr-") && name.endsWith(".service"))
      )
        continue;
      const path = join(directory, name);
      if ((await fs.stat(path)).size > 1024 * 1024)
        throw new Error("Service definition is too large to check safely");
      // Covers UTF-16 strings in binary launchd plists as well as ordinary
      // XML/systemd files without interpreting or printing their contents.
      const text = (await fs.readFile(path))
        .toString("utf8")
        .replaceAll("\0", "");
      for (const digest of text.match(/[a-f0-9]{64}/g) || [])
        retained.add(join(root, digest));
    }
  }
  return retained;
}
/** Inspect the user's executable paths, including restricted Linux session processes. */
export async function linuxEngineProcesses(root: string, procRoot = "/proc") {
  const inUse = new Set<string>();
  const engineNames = new Set(ENGINE_BINARIES.map((name) => name.slice(0, 15)));
  for (const pid of await fs.readdir(procRoot)) {
    if (!/^\d+$/.test(pid)) continue;
    const processRoot = join(procRoot, pid);
    try {
      if ((await fs.stat(processRoot)).uid !== process.getuid!()) continue;
      let executable: string;
      try {
        executable = await fs.readlink(join(processRoot, "exe"));
      } catch (error: any) {
        if (error.code !== "EACCES" && error.code !== "EPERM") throw error;
        // systemd's same-UID (sd-pam) process is deliberately nondumpable.
        // comm remains readable. Preserve every engine if an inaccessible
        // process could be one of our binaries, or cannot be identified.
        // Linux truncates executable names in comm to 15 bytes.
        const name = (
          await fs.readFile(join(processRoot, "comm"), "utf8")
        ).trim();
        if (!name || engineNames.has(name)) throw error;
        continue;
      }
      if (executable.startsWith(root + "/")) inUse.add(dirname(executable));
    } catch (error: any) {
      if (error.code !== "ENOENT" && error.code !== "ESRCH") throw error;
    }
  }
  return inUse;
}

export async function cleanupEngines(
  root: string,
  current: string,
  extraServiceDirectories: string[] = [],
) {
  root = realpathSync(root);
  current = realpathSync(current);
  const inUse = await referencedEngines(root, [
    join(homedir(), "Library/LaunchAgents"),
    "/Library/LaunchDaemons",
    join(
      process.env.XDG_CONFIG_HOME || join(homedir(), ".config"),
      "systemd/user",
    ),
    "/etc/systemd/user",
    "/etc/systemd/system",
    ...extraServiceDirectories,
  ]);
  if (process.platform === "darwin") {
    const { stdout } = await promisify(execFile)(
      "/usr/sbin/lsof",
      ["-a", "-u", String(process.getuid!()), "-d", "txt", "-F", "n"],
      { timeout: 15000, maxBuffer: 8 * 1024 * 1024, encoding: "utf8" },
    );
    for (const line of stdout.split("\n"))
      if (line.startsWith("n" + root + "/")) inUse.add(dirname(line.slice(1)));
  } else if (process.platform === "linux") {
    for (const engine of await linuxEngineProcesses(root)) inUse.add(engine);
  } else return;
  // If process enumeration fails, the caller retains every engine. Never remove
  // a running hub's executable merely because it came from an older app build.
  pruneEngines(root, current, inUse);
}
