import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { it, expect, vi } from "vitest";
import {
  mkdtempSync,
  writeFileSync,
  rmSync,
  statSync,
  utimesSync,
  mkdirSync,
  existsSync,
  copyFileSync,
  chmodSync,
  symlinkSync,
  promises as fs,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import {
  fileDigest,
  EngineDigests,
  pruneEngines,
  cleanupEngines,
  referencedEngines,
  linuxEngineProcesses,
  ENGINE_BINARIES,
  SHIPPED_DIGESTS,
  readShippedDigests,
  stageEngines,
  streamDigest,
} from "../desktop/engines";
it("preserves the engine identity across chunk boundaries and detects cache tampering", () => {
  const dir = mkdtempSync(join(tmpdir(), "twr-engine-hash-"));
  try {
    const first = Buffer.alloc(2 * 1024 * 1024 + 17, 41),
      second = Buffer.from("gateway");
    writeFileSync(join(dir, "engine"), first);
    writeFileSync(join(dir, "gateway"), second);
    const bundle = createHash("sha256");
    const expected = createHash("sha256").update(first).digest("hex");
    expect(fileDigest(join(dir, "engine"), bundle)).toBe(expected);
    fileDigest(join(dir, "gateway"), bundle);
    expect(bundle.digest("hex")).toBe(
      createHash("sha256").update(first).update(second).digest("hex"),
    );
    first[first.length - 1] = 42;
    writeFileSync(join(dir, "engine"), first);
    expect(fileDigest(join(dir, "engine"))).not.toBe(expected);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

it("cached engine digests detect changed content even when mtime is restored", async () => {
  const dir = mkdtempSync(join(tmpdir(), "twr-digest-cache-"));
  try {
    const path = join(dir, "engine"),
      index = join(dir, "digests.json");
    writeFileSync(path, "original");
    const first = new EngineDigests(index);
    const digest = await first.digest(path);
    first.save();
    const old = statSync(path);
    expect(await new EngineDigests(index).digest(path)).toBe(digest);
    writeFileSync(path, "modified");
    utimesSync(path, old.atime, old.mtime);
    expect(await new EngineDigests(index).digest(path)).not.toBe(digest);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
function engineSource(root: string, name: string) {
  const source = join(root, name);
  mkdirSync(source);
  for (const engine of ENGINE_BINARIES)
    writeFileSync(join(source, engine), `${engine} build`);
  return source;
}
const sha = (text: string) => createHash("sha256").update(text).digest("hex");

it("streams the same digest as the synchronous reader", async () => {
  const dir = mkdtempSync(join(tmpdir(), "twr-engine-stream-"));
  try {
    writeFileSync(join(dir, "engine"), Buffer.alloc(3 * 1024 * 1024 + 5, 7));
    expect(await streamDigest(join(dir, "engine"))).toBe(
      fileDigest(join(dir, "engine")),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

it("stages engines into a directory named by their contents, from any mount path", async () => {
  const root = mkdtempSync(join(tmpdir(), "twr-engine-stage-"));
  try {
    const first = engineSource(root, "mount-a");
    const engines = join(root, "engines");
    const stable = await stageEngines(first, engines);
    const expected = createHash("sha256")
      .update(
        JSON.stringify(
          ENGINE_BINARIES.map((name) => [name, sha(`${name} build`)]),
        ),
      )
      .digest("hex");
    expect(stable).toBe(join(engines, expected));
    for (const name of ENGINE_BINARIES)
      expect(await streamDigest(join(stable, name))).toBe(sha(`${name} build`));
    // An AppImage mounts the same build at a new path: the shipped digests name
    // the same directory without hashing the mounted engines.
    const second = engineSource(root, "mount-b");
    writeFileSync(
      join(second, SHIPPED_DIGESTS),
      JSON.stringify(
        Object.fromEntries(
          ENGINE_BINARIES.map((name) => [name, sha(`${name} build`)]),
        ),
      ),
    );
    expect(readShippedDigests(second)?.get("twr")).toBe(sha("twr build"));
    expect(await stageEngines(second, engines, true)).toBe(stable);
    // A damaged staged copy is replaced.
    writeFileSync(join(stable, "twr"), "tampered");
    expect(await stageEngines(second, engines, true)).toBe(stable);
    expect(await streamDigest(join(stable, "twr"))).toBe(sha("twr build"));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

it("ignores shipped digests that do not match the engines beside them", async () => {
  const root = mkdtempSync(join(tmpdir(), "twr-engine-manifest-"));
  try {
    const source = engineSource(root, "mount");
    const engines = join(root, "engines");
    writeFileSync(
      join(source, SHIPPED_DIGESTS),
      JSON.stringify(
        Object.fromEntries(ENGINE_BINARIES.map((name) => [name, sha(name)])),
      ),
    );
    const stable = await stageEngines(source, engines, true);
    const honest = await stageEngines(source, join(root, "honest"));
    expect(stable.slice(engines.length)).toBe(
      honest.slice(join(root, "honest").length),
    );
    expect(await streamDigest(join(stable, "twr"))).toBe(sha("twr build"));
    // No half-copied scratch files remain in the rejected directory.
    const rejected = createHash("sha256")
      .update(JSON.stringify(ENGINE_BINARIES.map((name) => [name, sha(name)])))
      .digest("hex");
    expect(
      (await fs.readdir(join(engines, rejected))).filter((name) =>
        name.startsWith("."),
      ),
    ).toEqual([]);
    // Malformed manifests are not trusted at all.
    writeFileSync(join(source, SHIPPED_DIGESTS), '{"twr":"nope"}');
    expect(readShippedDigests(source)).toBeUndefined();
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

it("engine cleanup retains running hubs and two recent versions", () => {
  const root = mkdtempSync(join(tmpdir(), "twr-engine-retain-"));
  try {
    const dirs = Array.from({ length: 5 }, (_, i) =>
      join(root, String(i).repeat(64)),
    );
    dirs.forEach((dir, i) => {
      mkdirSync(dir);
      utimesSync(dir, new Date(i * 1000), new Date(i * 1000));
    });
    pruneEngines(root, dirs[4], new Set([dirs[0]]));
    expect(dirs.map((path) => existsSync(path))).toEqual([
      true,
      false,
      false,
      true,
      true,
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

it("retains an engine used by a live OS process and prunes it after exit", async () => {
  const root = mkdtempSync(join(tmpdir(), "twr-active-engine-"));
  let process;
  try {
    const dirs = Array.from({ length: 4 }, (_, i) =>
      join(root, String(i).repeat(64)),
    );
    dirs.forEach((dir, i) => {
      mkdirSync(dir);
      utimesSync(dir, new Date(i * 1000), new Date(i * 1000));
    });
    const executable = join(dirs[0], "twr");
    copyFileSync("/bin/sleep", executable);
    chmodSync(executable, 0o755);
    // Current macOS can kill a copied platform binary after spawn. Give the
    // disposable fixture its own valid identity before checking live retention.
    if (globalThis.process.platform === "darwin")
      execFileSync(
        "/usr/bin/codesign",
        ["--force", "--sign", "-", executable],
        { stdio: "ignore" },
      );
    utimesSync(dirs[0], new Date(0), new Date(0));
    // On some Linux systems /bin/sleep is a multi-call binary that picks its
    // program by the name it is run as, so keep that name while the file is twr.
    process = spawn(executable, ["60"], { stdio: "ignore", argv0: "sleep" });
    const exited = once(process, "exit");
    await once(process, "spawn");
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(process.exitCode).toBeNull();
    expect(process.signalCode).toBeNull();
    await cleanupEngines(root, dirs[3]);
    expect(existsSync(dirs[0])).toBe(true);
    if (process.exitCode === null && process.signalCode === null)
      process.kill("SIGKILL");
    await exited;
    await cleanupEngines(root, dirs[3]);
    expect(existsSync(dirs[0])).toBe(false);
  } finally {
    if (process?.exitCode === null) process.kill("SIGKILL");
    rmSync(root, { recursive: true, force: true });
  }
}, 30000);

it("keeps engines referenced by stopped service units, including binary-plist strings", async () => {
  const root = mkdtempSync(join(tmpdir(), "twr-service-engines-"));
  try {
    const units = join(root, "units");
    mkdirSync(units);
    const a = "a".repeat(64),
      b = "b".repeat(64);
    writeFileSync(
      join(units, "twr-runner-lab.service"),
      "ExecStart=/old/symlink/engines/" + a + "/twr",
    );
    writeFileSync(
      join(units, "tools.twr.runner.lab.plist"),
      Buffer.from("/different/engines/" + b + "/twr", "utf16le"),
    );
    const c = "c".repeat(64),
      d = "d".repeat(64);
    writeFileSync(
      join(units, "twr-speak.service"),
      "ExecStart=/engines/" + c + "/twr",
    );
    writeFileSync(
      join(units, "tools.twr.speak.plist"),
      "/engines/" + d + "/twr",
    );
    const retained = await referencedEngines(root, [
      units,
      join(root, "missing"),
    ]);
    expect([...retained].sort()).toEqual([
      join(root, a),
      join(root, b),
      join(root, c),
      join(root, d),
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

it.each(["(sd-pam)", ...ENGINE_BINARIES])(
  "handles inaccessible Linux executable %s without losing live engines",
  async (name) => {
    const root = mkdtempSync(join(tmpdir(), "twr-proc-inspection-"));
    const procRoot = join(root, "proc");
    const denied = join(procRoot, "101");
    const live = join(procRoot, "202");
    const engine = join(root, "engines", "a".repeat(64));
    mkdirSync(denied, { recursive: true });
    mkdirSync(live);
    writeFileSync(join(denied, "comm"), name.slice(0, 15) + "\n");
    symlinkSync(join(engine, "twr"), join(live, "exe"));
    const original = fs.readlink.bind(fs);
    const blocked = Object.assign(new Error("nondumpable process"), {
      code: "EACCES",
      syscall: "readlink",
      path: join(denied, "exe"),
    });
    const spy = vi.spyOn(fs, "readlink").mockImplementation((async (
      path: any,
      ...args: any[]
    ) => {
      if (String(path) === blocked.path) throw blocked;
      return (original as any)(path, ...args);
    }) as typeof fs.readlink);
    try {
      if (name === "(sd-pam)")
        expect([
          ...(await linuxEngineProcesses(join(root, "engines"), procRoot)),
        ]).toEqual([engine]);
      else
        await expect(
          linuxEngineProcesses(join(root, "engines"), procRoot),
        ).rejects.toBe(blocked);
      // Unidentifiable processes still prevent cleanup.
      writeFileSync(join(denied, "comm"), "\n");
      await expect(
        linuxEngineProcesses(join(root, "engines"), procRoot),
      ).rejects.toBe(blocked);
    } finally {
      spy.mockRestore();
      rmSync(root, { recursive: true, force: true });
    }
  },
);
