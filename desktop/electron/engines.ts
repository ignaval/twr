import { createHash, type Hash } from "node:crypto";
import { openSync, readSync, closeSync } from "node:fs";

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
