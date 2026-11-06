import { it, expect } from "vitest";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { fileDigest } from "../electron/engines";
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
