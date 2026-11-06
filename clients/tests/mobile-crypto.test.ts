import { describe, it, expect, vi } from "vitest";
import { webcrypto } from "node:crypto";
import { fileDigest, requestId } from "../mobile/src/crypto";
describe("mobile custom-scheme crypto", () => {
  it("yields while hashing a large attachment and matches native SHA-256", async () => {
    const data = new Uint8Array(5 * 1024 * 1024 + 17);
    data.fill(37);
    const expected = Buffer.from(
      await webcrypto.subtle.digest("SHA-256", data),
    ).toString("hex");
    vi.stubGlobal("crypto", {});
    let ticked = false;
    const timer = setTimeout(() => {
      ticked = true;
    }, 0);
    try {
      expect(await fileDigest(data.buffer)).toBe(expected);
      expect(ticked).toBe(true);
    } finally {
      clearTimeout(timer);
      vi.unstubAllGlobals();
    }
  });
  it("uses OS entropy and SHA-256 without secure-context methods", async () => {
    vi.stubGlobal("crypto", {
      getRandomValues: webcrypto.getRandomValues.bind(webcrypto),
    });
    try {
      const ids = new Set(Array.from({ length: 100 }, requestId));
      expect(ids.size).toBe(100);
      for (const id of ids)
        expect(id).toMatch(
          /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
        );
      expect(await fileDigest(new TextEncoder().encode("abc").buffer)).toBe(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
      );
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
