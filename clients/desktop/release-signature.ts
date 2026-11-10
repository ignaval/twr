/**
 * The desktop installers' signed checksums. A stable release lists the SHA-256 of every
 * desktop installer in `desktop-sha256sums.txt` (first line `# twr desktop release <tag>`)
 * and signs that file with the release key into `desktop-sha256sums.txt.sig` (base64 of a
 * raw Ed25519 signature; .github/workflows/release.yml). The AppImage updater installs only
 * an image whose SHA-256 that list names, so a release whose assets alone were replaced
 * (the AppImage and latest-linux.yml with its SHA-512 together) is refused.
 */
import {
  createHash,
  createPublicKey,
  verify,
  type KeyObject,
} from "node:crypto";
import { closeSync, openSync, readSync } from "node:fs";

/**
 * The public half of the release key: release/signing-key.pub, which `twr update`
 * (RELEASE_KEY in crates/twr-cli/src/main.rs) and install.sh also check against. A test
 * (tests/release-signature.test.ts) fails when this copy differs from that file.
 */
export const RELEASE_SIGNING_KEY = `-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAoEFCsIDDaJTSlpcteH2pp3aZpLQ+SB+18sUdZNqe9dg=
-----END PUBLIC KEY-----
`;

export const SIGNED_SUMS = "desktop-sha256sums.txt";
export const SIGNED_SUMS_SIGNATURE = `${SIGNED_SUMS}.sig`;
/** A checksum list is a few hundred bytes; anything far larger is not one. */
export const MAX_SUMS_BYTES = 64 * 1024;

export class SignatureError extends Error {}

export interface SignedSums {
  /** The release tag the list names, such as `v0.4.0`. */
  tag: string;
  /** Asset name to lower-case hex SHA-256. */
  sums: Map<string, string>;
}

/**
 * Checks `signature` (the text of desktop-sha256sums.txt.sig) over the exact bytes of
 * `text` with the release key, and that the list names `tag`; returns its entries.
 */
export function verifySignedSums(
  text: Buffer,
  signature: string,
  tag: string,
  key: KeyObject | string = RELEASE_SIGNING_KEY,
): SignedSums {
  if (text.length > MAX_SUMS_BYTES)
    throw new SignatureError(`${SIGNED_SUMS} is too large`);
  const raw = Buffer.from(signature.trim(), "base64");
  if (raw.length !== 64 || !/^[A-Za-z0-9+/]+={0,2}$/.test(signature.trim()))
    throw new SignatureError(
      `${SIGNED_SUMS_SIGNATURE} is not an Ed25519 signature`,
    );
  const publicKey = typeof key === "string" ? createPublicKey(key) : key;
  if (publicKey.asymmetricKeyType !== "ed25519")
    throw new SignatureError("The release key is not an Ed25519 key");
  if (!verify(null, text, publicKey, raw))
    throw new SignatureError(
      `${SIGNED_SUMS} is not signed with the twr release key`,
    );
  const lines = text.toString("utf8").split("\n");
  const header = /^# twr desktop release (\S+)$/.exec(lines[0] ?? "");
  if (!header)
    throw new SignatureError(`${SIGNED_SUMS} does not name its release`);
  if (header[1] !== tag)
    throw new SignatureError(`${SIGNED_SUMS} is for ${header[1]}, not ${tag}`);
  const sums = new Map<string, string>();
  for (const line of lines.slice(1)) {
    if (!line) continue;
    const entry = /^([0-9a-f]{64}) [ *]?(\S+)$/.exec(line);
    if (!entry) throw new SignatureError(`${SIGNED_SUMS} has a malformed line`);
    sums.set(entry[2], entry[1]);
  }
  return { tag: header[1], sums };
}

/** The SHA-256 of a file, read in 1 MiB pieces. */
export function fileSha256(path: string): string {
  const digest = createHash("sha256");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  const file = openSync(path, "r");
  try {
    for (;;) {
      const count = readSync(file, buffer, 0, buffer.length, null);
      if (!count) break;
      digest.update(buffer.subarray(0, count));
    }
  } finally {
    closeSync(file);
  }
  return digest.digest("hex");
}

/**
 * Throws unless `path`, downloaded as the asset `name` of version `version`, is listed
 * with its SHA-256 in `signed`, the verified list of that version's release.
 */
export function checkSignedDownload(
  signed: SignedSums | undefined,
  version: string,
  name: string,
  path: string,
) {
  if (!signed)
    throw new SignatureError(
      "This update has no verified checksums; twr installs only signed updates.",
    );
  if (signed.tag.replace(/^v/, "") !== version.replace(/^v/, ""))
    throw new SignatureError(
      `The signed checksums are for ${signed.tag}, not ${version}.`,
    );
  const expected = signed.sums.get(name);
  if (!expected)
    throw new SignatureError(
      `${name} is not in the release's signed checksums.`,
    );
  if (fileSha256(path) !== expected)
    throw new SignatureError(
      `The downloaded ${name} does not match the release's signed checksum.`,
    );
}
