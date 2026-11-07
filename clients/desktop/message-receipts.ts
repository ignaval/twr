import { createHash } from "node:crypto";
import type { SecureStore } from "./secure-store";
import type { DurableMessage } from "../shared/offline";
import { RECEIPT_WINDOW } from "../shared/offline-constants";

interface Receipt {
  hash: string;
  createdAt: number;
  reply?: { result?: unknown; error?: string };
}
/** Claim before contacting a hub. A crash between claim and reply requires human review. */
export class MessageReceipts {
  private tail: Promise<unknown> = Promise.resolve();
  private inFlight = new Map<string, { hash: string; promise: Promise<any> }>();
  constructor(private vault: Pick<SecureStore, "read" | "write">) {}
  send(message: DurableMessage, deliver: () => Promise<any>) {
    const hash = createHash("sha256")
      .update(
        JSON.stringify([message.workspace, message.method, message.params]),
      )
      .digest("hex");
    const duplicate = this.inFlight.get(message.id);
    if (duplicate)
      return duplicate.hash === hash
        ? duplicate.promise
        : Promise.reject(
            new Error("Message ID belongs to a different operation"),
          );
    const serialized = <T>(job: () => Promise<T>) => {
      const next = this.tail.catch(() => {}).then(job);
      this.tail = next;
      return next;
    };
    const operation = (async () => {
      if (
        Date.now() - message.createdAt >= RECEIPT_WINDOW ||
        message.createdAt > Date.now() + 60_000
      )
        throw new Error(
          "Message is outside the safe retry window. Check its delivery before trying again.",
        );
      const prior = await serialized(async () => {
        const receipts: Record<string, Receipt> = JSON.parse(
          (await this.vault.read("messages")) || "{}",
        );
        const existing = receipts[message.id];
        if (existing) {
          if (existing.hash !== hash)
            throw new Error("Message ID belongs to a different operation");
          return existing;
        }
        const recent = Object.fromEntries(
          Object.entries(receipts).filter(
            ([, receipt]) =>
              Date.now() - receipt.createdAt < 24 * 60 * 60 * 1000,
          ),
        );
        if (Object.keys(recent).length >= 4096)
          throw new Error("Daily message receipt limit reached");
        recent[message.id] = { hash, createdAt: Date.now() };
        await this.vault.write("messages", JSON.stringify(recent));
        return undefined;
      });
      if (prior) {
        if (!prior.reply)
          throw new Error(
            "This message was started but its outcome is unknown. Check the conversation before trying a new message.",
          );
        if (prior.reply.error)
          throw new Error("REQUEST_REJECTED: " + prior.reply.error);
        return prior.reply.result;
      }
      let reply: Receipt["reply"];
      try {
        reply = { result: await deliver() };
      } catch (error) {
        const failure = error as Error & { delivery?: string };
        if (failure.delivery === "not-sent") {
          await serialized(async () => {
            const receipts = JSON.parse(
              (await this.vault.read("messages")) || "{}",
            );
            delete receipts[message.id];
            await this.vault.write("messages", JSON.stringify(receipts));
          });
          throw error;
        }
        if (failure.delivery === "rejected") {
          reply = { error: failure.message };
        } else {
          throw new Error(
            "Message outcome is unknown. Check the conversation before trying a new message.",
            { cause: error },
          );
        }
      }
      await serialized(async () => {
        const receipts: Record<string, Receipt> = JSON.parse(
          (await this.vault.read("messages")) || "{}",
        );
        receipts[message.id] = { hash, createdAt: Date.now(), reply };
        await this.vault.write("messages", JSON.stringify(receipts));
      });
      if (reply.error) throw new Error("REQUEST_REJECTED: " + reply.error);
      return reply.result;
    })().finally(() => this.inFlight.delete(message.id));
    this.inFlight.set(message.id, { hash, promise: operation });
    return operation;
  }
}
