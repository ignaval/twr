// Runs in a worker thread (pair-proof.ts): 64 MiB of Argon2id would stall the main
// process, and with it every window's IPC, for about a second.
import { parentPort, workerData } from "node:worker_threads";
import { argon2id } from "@noble/hashes/argon2.js";

const { password, salt, m, t, p, dkLen } = workerData as {
  password: Uint8Array;
  salt: Uint8Array;
  m: number;
  t: number;
  p: number;
  dkLen: number;
};
try {
  parentPort!.postMessage({
    ok: argon2id(password, salt, { m, t, p, dkLen, version: 0x13 }),
  });
} catch (error) {
  parentPort!.postMessage({ error: String(error) });
}
