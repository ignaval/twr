import { Worker } from "node:worker_threads";
import { createHmac, hkdfSync, timingSafeEqual } from "node:crypto";
import { argon2idAsync } from "@noble/hashes/argon2.js";
import { base64UrlDecode, normalizeCode } from "../shared/phone-pairing";

/** The pin proof of typed pairing (docs/design/phone-pairing.md › The proof key). */
const SALT = "twr-mobile-pair-v2";
const LABEL = Buffer.from("twr-mobile-pin-v2\0", "latin1");
export const ARGON2 = { m: 65536, t: 3, p: 1, dkLen: 32 } as const;

/** The bundled argon2-worker.ts, as source (scripts/build-host.mjs). Undefined unbundled
 * (tests, tsc), where the async variant yields to the event loop instead. */
declare const __TWR_ARGON2_WORKER__: string | undefined;

/** Argon2id of the normalized code: 64 MiB, 3 passes, one lane, version 0x13, 32 bytes.
 * In a worker thread, so the main process keeps serving every window meanwhile. */
export function stretchCode(code: string): Promise<Uint8Array> {
  const normalized = normalizeCode(code);
  if (!normalized) return Promise.reject(new Error("Not a pairing code"));
  const password = new TextEncoder().encode(normalized);
  const salt = new TextEncoder().encode(SALT);
  const source =
    typeof __TWR_ARGON2_WORKER__ === "string" ? __TWR_ARGON2_WORKER__ : "";
  if (!source)
    return argon2idAsync(password, salt, {
      ...ARGON2,
      version: 0x13,
      asyncTick: 10,
    });
  return new Promise((resolve, reject) => {
    let settled = false;
    const worker = new Worker(source, {
      eval: true,
      workerData: { password, salt, ...ARGON2 },
    });
    const finish = (error?: Error, value?: Uint8Array) => {
      if (settled) return;
      settled = true;
      void worker.terminate();
      if (error) reject(error);
      else resolve(value!);
    };
    worker.once("message", (message: { ok?: Uint8Array; error?: string }) =>
      message.ok
        ? finish(undefined, new Uint8Array(message.ok))
        : finish(new Error(message.error || "The pairing KDF failed")),
    );
    worker.once("error", (error: Error) => finish(error));
    worker.once("exit", () => finish(new Error("The pairing KDF stopped")));
  });
}

/** `K_proof = HKDF-SHA256(salt, ikm = stretched, info = "pin-proof", 32)`. */
export function proofKey(stretched: Uint8Array): Buffer {
  return Buffer.from(hkdfSync("sha256", stretched, SALT, "pin-proof", 32));
}

/** `HMAC-SHA256(K_proof, "twr-mobile-pin-v2" || 0x00 || pin || nonce || gateway_id)`. */
export function pinProof(
  key: Uint8Array,
  pin: string,
  nonce: Uint8Array,
  gatewayId: string,
): Buffer {
  const raw = base64UrlDecode(pin);
  if (raw.length !== 32) throw new Error("A pin is 32 bytes");
  if (nonce.length !== 32) throw new Error("A nonce is 32 bytes");
  return createHmac("sha256", key)
    .update(LABEL)
    .update(raw)
    .update(nonce)
    .update(Buffer.from(gatewayId, "utf8"))
    .digest();
}

/** Checks a served proof in constant time. */
export function proofMatches(
  key: Uint8Array,
  pin: string,
  nonce: Uint8Array,
  gatewayId: string,
  proof: Uint8Array,
): boolean {
  const expected = pinProof(key, pin, nonce, gatewayId);
  return (
    proof.length === expected.length &&
    timingSafeEqual(expected, Buffer.from(proof))
  );
}
