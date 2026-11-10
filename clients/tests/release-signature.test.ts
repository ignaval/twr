import { describe, expect, it } from "vitest";
import {
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  sign,
  createHash,
} from "node:crypto";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  RELEASE_SIGNING_KEY,
  checkSignedDownload,
  verifySignedSums,
} from "../desktop/release-signature";
import { fetchSignedSums } from "../desktop/updater";

const root = join(__dirname, "../..");
// The key the CLI's tests sign with (crates/twr-cli/tests/fixtures/signing).
const privateKey = createPrivateKey(
  readFileSync(
    join(root, "crates/twr-cli/tests/fixtures/signing/test-key.pem"),
    "utf8",
  ),
);
const publicKey = createPublicKey(privateKey);
// As the release workflow writes it: `openssl pkeyutl -sign -rawin | base64 -w0`.
const signed = (text: string, key = privateKey) =>
  sign(null, Buffer.from(text), key).toString("base64");
const image = "PRETEND APPIMAGE";
const sha = createHash("sha256").update(image).digest("hex");
const sums = `# twr desktop release v0.4.0\n${sha}  twr-0.4.0.AppImage\n${"a".repeat(64)}  twr-desktop_0.4.0_amd64.deb\n`;

describe("release signature", () => {
  it("embeds the key in release/signing-key.pub", () => {
    expect(RELEASE_SIGNING_KEY).toBe(
      readFileSync(join(root, "release/signing-key.pub"), "utf8"),
    );
    expect(createPublicKey(RELEASE_SIGNING_KEY).asymmetricKeyType).toBe(
      "ed25519",
    );
  });

  it("reads a list signed with the key, for the release it names", () => {
    const result = verifySignedSums(
      Buffer.from(sums),
      signed(sums),
      "v0.4.0",
      publicKey,
    );
    expect(result.tag).toBe("v0.4.0");
    expect(result.sums.get("twr-0.4.0.AppImage")).toBe(sha);
    expect(result.sums.size).toBe(2);
  });

  it("refuses another key, a changed list, another release and bad signatures", () => {
    const other = generateKeyPairSync("ed25519").privateKey;
    expect(() =>
      verifySignedSums(
        Buffer.from(sums),
        signed(sums, other),
        "v0.4.0",
        publicKey,
      ),
    ).toThrow(/not signed with the twr release key/);
    // The real release key never signed this list.
    expect(() =>
      verifySignedSums(Buffer.from(sums), signed(sums), "v0.4.0"),
    ).toThrow(/not signed with the twr release key/);
    const changed = sums.replace(sha, "b".repeat(64));
    expect(() =>
      verifySignedSums(Buffer.from(changed), signed(sums), "v0.4.0", publicKey),
    ).toThrow(/not signed/);
    // A genuine older list replayed as a newer release.
    expect(() =>
      verifySignedSums(Buffer.from(sums), signed(sums), "v0.5.0", publicKey),
    ).toThrow(/is for v0.4.0, not v0.5.0/);
    for (const signature of [
      "",
      "not base64!",
      Buffer.alloc(32).toString("base64"),
    ])
      expect(() =>
        verifySignedSums(Buffer.from(sums), signature, "v0.4.0", publicKey),
      ).toThrow(/not an Ed25519 signature/);
    const unnamed = `${sha}  twr-0.4.0.AppImage\n`;
    expect(() =>
      verifySignedSums(
        Buffer.from(unnamed),
        signed(unnamed),
        "v0.4.0",
        publicKey,
      ),
    ).toThrow(/does not name its release/);
  });

  it("accepts only a download whose SHA-256 the signed list names", () => {
    const directory = mkdtempSync(join(tmpdir(), "twr-signed-"));
    const file = join(directory, "twr-0.4.0.AppImage");
    writeFileSync(file, image);
    const list = verifySignedSums(
      Buffer.from(sums),
      signed(sums),
      "v0.4.0",
      publicKey,
    );
    expect(() =>
      checkSignedDownload(list, "0.4.0", "twr-0.4.0.AppImage", file),
    ).not.toThrow();
    expect(() =>
      checkSignedDownload(undefined, "0.4.0", "twr-0.4.0.AppImage", file),
    ).toThrow(/only signed updates/);
    expect(() =>
      checkSignedDownload(list, "0.5.0", "twr-0.4.0.AppImage", file),
    ).toThrow(/for v0.4.0, not 0.5.0/);
    expect(() =>
      checkSignedDownload(list, "0.4.0", "twr-0.4.1.AppImage", file),
    ).toThrow(/not in the release's signed checksums/);
    writeFileSync(file, "SWAPPED APPIMAGE");
    expect(() =>
      checkSignedDownload(list, "0.4.0", "twr-0.4.0.AppImage", file),
    ).toThrow(/does not match/);
  });

  it("fetches the list and its signature from the release, or reports none", async () => {
    const assets = [
      { name: "desktop-sha256sums.txt", url: "https://example.invalid/sums" },
      {
        name: "desktop-sha256sums.txt.sig",
        url: "https://example.invalid/sig",
      },
    ];
    const get = (async (url: string) =>
      new Response(
        url.endsWith("/sums") ? sums : signed(sums),
      )) as typeof fetch;
    // The real key did not sign the test list: a release signed with another key fails.
    await expect(fetchSignedSums(get, "v0.4.0", assets)).rejects.toThrow(
      /not signed with the twr release key/,
    );
    expect(await fetchSignedSums(get, "v0.4.0", assets.slice(0, 1))).toBe(
      undefined,
    );
    const missing = (async () =>
      new Response("", { status: 404 })) as typeof fetch;
    await expect(fetchSignedSums(missing, "v0.4.0", assets)).rejects.toThrow(
      /GitHub answered 404/,
    );
  });
});
