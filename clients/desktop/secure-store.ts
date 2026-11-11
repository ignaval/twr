import { safeStorage } from "electron";
import { createHash, randomUUID } from "node:crypto";
import { constants } from "node:fs";
import * as fs from "node:fs/promises";
import path from "node:path";

const MAX_VALUE = 8 * 1024 * 1024;
const MAX_TOTAL = 64 * 1024 * 1024;
export interface Cryptor {
  isEncryptionAvailable(): boolean | Promise<boolean>;
  getSelectedStorageBackend?(): string;
  encryptString(value: string): Buffer | Promise<Buffer>;
  decryptString(
    value: Buffer,
  ):
    | string
    | { result: string; shouldReEncrypt?: boolean }
    | Promise<string | { result: string; shouldReEncrypt?: boolean }>;
}

/** State is encrypted with the OS vault. A plaintext Linux fallback is never accepted. */
export class SecureStore {
  private tail: Promise<unknown> = Promise.resolve();
  constructor(
    private directory: string,
    private cryptor: Cryptor = {
      isEncryptionAvailable: () => safeStorage.isAsyncEncryptionAvailable(),
      getSelectedStorageBackend: () =>
        process.platform === "linux"
          ? safeStorage.getSelectedStorageBackend()
          : "system",
      encryptString: (value) => safeStorage.encryptStringAsync(value),
      decryptString: (value) => safeStorage.decryptStringAsync(value),
    },
  ) {}
  async ensureAvailable() {
    if (
      !(await this.cryptor.isEncryptionAvailable()) ||
      this.cryptor.getSelectedStorageBackend?.() === "basic_text"
    )
      throw new Error(
        "Encrypted storage needs an unlocked system keychain. Enable a keyring to save hubs or offline history.",
      );
  }
  private location(key: string) {
    if (!key || Buffer.byteLength(key) > 512 || /[\x00-\x1f]/.test(key))
      throw new Error("Invalid storage key");
    return path.join(
      this.directory,
      createHash("sha256").update(key).digest("hex") + ".enc",
    );
  }
  private serial<T>(job: () => Promise<T>): Promise<T> {
    const result = this.tail.catch(() => {}).then(job);
    this.tail = result;
    return result;
  }
  private async prepare() {
    await this.ensureAvailable();
    await fs.mkdir(this.directory, { recursive: true, mode: 0o700 });
    const dir = await fs.lstat(this.directory);
    if (!dir.isDirectory() || dir.isSymbolicLink())
      throw new Error("Unsafe encrypted storage directory");
    await fs.chmod(this.directory, 0o700);
  }
  read(key: string): Promise<string | null> {
    return this.serial(async () => {
      const filename = this.location(key);
      await this.prepare();
      let file;
      try {
        file = await fs.open(
          filename,
          constants.O_RDONLY | constants.O_NOFOLLOW,
        );
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
        throw error;
      }
      try {
        const stat = await file.stat();
        if (!stat.isFile() || stat.size > MAX_VALUE * 3)
          throw new Error("Invalid encrypted state file");
        const decrypted = await this.cryptor.decryptString(
          await file.readFile(),
        );
        const plaintext =
          typeof decrypted === "string" ? decrypted : decrypted.result;
        const record = JSON.parse(plaintext);
        if (
          record.version !== 1 ||
          record.key !== key ||
          typeof record.value !== "string"
        )
          throw new Error("Encrypted state belongs to a different workspace");
        if (typeof decrypted !== "string" && decrypted.shouldReEncrypt)
          await this.persist(
            filename,
            await this.cryptor.encryptString(plaintext),
          );
        return record.value;
      } finally {
        await file.close();
      }
    });
  }
  write(key: string, value: string): Promise<void> {
    return this.serial(async () => {
      const filename = this.location(key);
      if (typeof value !== "string" || Buffer.byteLength(value) > MAX_VALUE)
        throw new Error("Offline state is too large (8 MiB limit)");
      await this.prepare();
      const encrypted = await this.cryptor.encryptString(
        JSON.stringify({ version: 1, key, value }),
      );
      await this.persist(filename, encrypted);
    });
  }
  private async persist(filename: string, encrypted: Buffer) {
    const entries = await fs.readdir(this.directory);
    let total = encrypted.length;
    for (const entry of entries) {
      if (
        !/^[a-f0-9]{64}\.enc$/.test(entry) ||
        path.join(this.directory, entry) === filename
      )
        continue;
      total += (await fs.lstat(path.join(this.directory, entry))).size;
    }
    if (total > MAX_TOTAL)
      throw new Error(
        "Offline storage is full (64 MiB). Clear saved history in Settings.",
      );
    const staged = path.join(this.directory, randomUUID() + ".part");
    try {
      const file = await fs.open(staged, "wx", 0o600);
      try {
        await file.writeFile(encrypted);
        await file.sync();
      } finally {
        await file.close();
      }
      await fs.rename(staged, filename);
    } finally {
      await fs.unlink(staged).catch(() => {});
    }
  }
  delete(key: string): Promise<void> {
    return this.serial(async () => {
      await this.prepare();
      await fs
        .unlink(this.location(key))
        .catch((error: NodeJS.ErrnoException) => {
          if (error.code !== "ENOENT") throw error;
        });
    });
  }
}
