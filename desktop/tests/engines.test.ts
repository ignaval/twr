import { it, expect } from "vitest";
import {
  mkdtempSync,
  writeFileSync,
  rmSync,
  statSync,
  utimesSync,
  mkdirSync,
  existsSync,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { fileDigest, EngineDigests, pruneEngines } from "../electron/engines";
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
