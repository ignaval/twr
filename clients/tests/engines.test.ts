import { spawn } from "node:child_process";
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

it("cached engine digests detect changed content even when mtime is restored", () => {
  const dir = mkdtempSync(join(tmpdir(), "twr-digest-cache-"));
  try {
    const path = join(dir, "engine"),
      index = join(dir, "digests.json");
    writeFileSync(path, "original");
    const first = new EngineDigests(index);
    const digest = first.digest(path);
    first.save();
    const old = statSync(path);
    expect(new EngineDigests(index).digest(path)).toBe(digest);
    writeFileSync(path, "modified");
    utimesSync(path, old.atime, old.mtime);
    expect(new EngineDigests(index).digest(path)).not.toBe(digest);
  } finally {
    rmSync(dir, { recursive: true, force: true });
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
    utimesSync(dirs[0], new Date(0), new Date(0));
    process = spawn(executable, ["60"], { stdio: "ignore" });
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
