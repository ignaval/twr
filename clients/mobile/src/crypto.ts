import { sha256 } from "@noble/hashes/sha2.js";

// WKWebView custom schemes need not expose secure-context Web Crypto methods.
// getRandomValues remains available and supplies OS entropy on both hosts.
export function requestId(): string {
  if (globalThis.crypto.randomUUID) return globalThis.crypto.randomUUID();
  const bytes = globalThis.crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(
    "",
  );
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
export async function fileDigest(bytes: ArrayBuffer): Promise<string> {
  let digest: Uint8Array;
  if (globalThis.crypto.subtle)
    digest = new Uint8Array(
      await globalThis.crypto.subtle.digest("SHA-256", bytes),
    );
  else {
    const hash = sha256.create();
    const data = new Uint8Array(bytes);
    const chunk = 1024 * 1024;
    for (let offset = 0; offset < data.length; offset += chunk) {
      hash.update(data.subarray(offset, offset + chunk));
      // WKWebView's custom scheme may require the JS fallback. Let input and
      // rendering run between chunks of a large attachment.
      if (offset + chunk < data.length)
        await new Promise((resolve) => setTimeout(resolve, 0));
    }
    digest = hash.digest();
  }
  return Array.from(digest, (b) => b.toString(16).padStart(2, "0")).join("");
}
