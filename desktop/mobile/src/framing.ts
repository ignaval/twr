import type { Json } from "../../src/types";

/** Bounded message reassembly; credentials and domain state stay in their hosts. */
export class FrameAssembler {
  private transfers = new Map<
    string,
    { count: number; next: number; bytes: number; parts: string[] }
  >();
  clear() {
    this.transfers.clear();
  }
  accept(message: Json): Json | undefined {
    if (message.type === "frame-cancel") {
      this.transfers.delete(message.transfer);
      return;
    }
    if (message.type !== "frame-part") return message;
    const { transfer, index, count, data } = message;
    if (
      typeof transfer !== "string" ||
      transfer.length > 80 ||
      !Number.isInteger(index) ||
      !Number.isInteger(count) ||
      count < 1 ||
      count > 1024 ||
      index < 0 ||
      index >= count ||
      typeof data !== "string"
    )
      throw new Error("Invalid hub message fragment");
    const bytes = new TextEncoder().encode(data).length;
    if (bytes > 32 * 1024) throw new Error("Hub fragment exceeds its limit");
    if (index === 0) {
      if (this.transfers.has(transfer) || this.transfers.size >= 4)
        throw new Error("Too many incomplete hub messages");
      this.transfers.set(transfer, { count, next: 0, bytes: 0, parts: [] });
    }
    const pending = this.transfers.get(transfer);
    if (!pending || pending.count !== count || pending.next !== index)
      throw new Error("Hub message fragments are out of order");
    pending.bytes += bytes;
    if (pending.bytes > 16 * 1024 * 1024)
      throw new Error("Hub message exceeds 16 MiB");
    pending.next++;
    pending.parts.push(data);
    if (pending.next !== pending.count) return;
    this.transfers.delete(transfer);
    const result = JSON.parse(pending.parts.join(""));
    if (!result || typeof result !== "object" || Array.isArray(result))
      throw new Error("Invalid hub message");
    return result;
  }
}
