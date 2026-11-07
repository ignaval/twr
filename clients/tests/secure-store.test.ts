import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createCipheriv,
  createDecipheriv,
  randomBytes,
  createHash,
} from "node:crypto";
import * as fs from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
vi.mock("electron", () => ({ safeStorage: {} }));
import { SecureStore, type Cryptor } from "../desktop/secure-store";
const directories: string[] = [];
afterEach(async () => {
  await Promise.all(
    directories
      .splice(0)
      .map((dir) => fs.rm(dir, { recursive: true, force: true })),
  );
});
function crypto(): Cryptor {
  const key = randomBytes(32);
  return {
    isEncryptionAvailable: () => true,
    getSelectedStorageBackend: () => "gnome_libsecret",
    encryptString(value) {
      const iv = randomBytes(12);
      const cipher = createCipheriv("aes-256-gcm", key, iv);
      return Buffer.concat([
        iv,
        cipher.update(value),
        cipher.final(),
        cipher.getAuthTag(),
      ]);
    },
    decryptString(bytes) {
      const decipher = createDecipheriv(
        "aes-256-gcm",
        key,
        bytes.subarray(0, 12),
      );
      decipher.setAuthTag(bytes.subarray(-16));
      return Buffer.concat([
        decipher.update(bytes.subarray(12, -16)),
        decipher.final(),
      ]).toString();
    },
  };
}
describe("native encrypted state files", () => {
  it("re-encrypts a record when the native key provider rotates its key", async () => {
    const dir = await fs.mkdtemp(path.join(tmpdir(), "twr-vault-"));
    directories.push(dir);
    const base = crypto();
    let rotate = false;
    const vault = new SecureStore(dir, {
      ...base,
      decryptString: async (bytes) => ({
        result: (await base.decryptString(bytes)) as string,
        shouldReEncrypt: rotate,
      }),
    });
    await vault.write("offline:rotation", "private");
    const filename = path.join(dir, (await fs.readdir(dir))[0]);
    const before = await fs.readFile(filename);
    rotate = true;
    expect(await vault.read("offline:rotation")).toBe("private");
    expect((await fs.readFile(filename)).equals(before)).toBe(false);
  });
  it("refuses a plaintext Linux backend before creating files", async () => {
    const dir = await fs.mkdtemp(path.join(tmpdir(), "twr-vault-"));
    directories.push(dir);
    const vault = new SecureStore(path.join(dir, "state"), {
      ...crypto(),
      getSelectedStorageBackend: () => "basic_text",
    });
    await expect(
      vault.write("offline:hub", "sensitive conversation"),
    ).rejects.toThrow("keyring");
    expect(await fs.readdir(dir)).toEqual([]);
  });
  it("encrypts, binds records to their namespace, and rejects symlinks", async () => {
    const dir = await fs.mkdtemp(path.join(tmpdir(), "twr-vault-"));
    directories.push(dir);
    const vault = new SecureStore(dir, crypto());
    await vault.write("offline:first", "private message");
    expect(await vault.read("offline:first")).toBe("private message");
    const name = (key: string) =>
      createHash("sha256").update(key).digest("hex") + ".enc";
    const encrypted = await fs.readFile(path.join(dir, name("offline:first")));
    expect(encrypted.includes(Buffer.from("private message"))).toBe(false);
    await fs.writeFile(path.join(dir, name("offline:second")), encrypted);
    await expect(vault.read("offline:second")).rejects.toThrow(
      "different workspace",
    );
    await fs.symlink(
      path.join(dir, name("offline:first")),
      path.join(dir, name("offline:third")),
    );
    await expect(vault.read("offline:third")).rejects.toThrow();
  });
});
