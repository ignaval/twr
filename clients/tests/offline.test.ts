import { describe, expect, it } from "vitest";
import {
  OfflineJournal,
  parseOffline,
  RECEIPT_WINDOW,
  forgetOfflineHost,
  offlineJournal,
  appendDraft,
} from "../shared/offline";
import { MessageReceipts } from "../desktop/message-receipts";
import type { DesktopAPI } from "../shared/types";

function storage() {
  const values = new Map<string, string>();
  return {
    values,
    async secureRead(key: string) {
      return values.get(key) ?? null;
    },
    async secureWrite(key: string, value: string) {
      values.set(key, value);
    },
    async secureDelete(key: string) {
      values.delete(key);
    },
  };
}
describe("encrypted offline recovery", () => {
  it("delivers older workspace messages before a newly composed message", async () => {
    const disk = storage();
    const calls: string[] = [];
    let finish!: () => void;
    const host = {
      ...disk,
      sendMessage: async (message: any) => {
        calls.push(message.params.message);
        if (calls.length === 1)
          await new Promise<void>((resolve) => {
            finish = resolve;
          });
        return { ok: true };
      },
    } as unknown as DesktopAPI;
    const journal = new OfflineJournal(host, "ordered-workspace");
    const a = await journal.enqueue("work", "say", { message: "A" });
    await journal.enqueue("work", "say", { message: "B" });
    const first = journal.deliver(a);
    await new Promise((resolve) => setTimeout(resolve, 0));
    const c = await journal.enqueue("work", "say", { message: "C" });
    const last = journal.deliver(c);
    finish();
    await Promise.all([first, last]);
    expect(calls).toEqual(["A", "B", "C"]);
  });
  it("does not send past an older transient delivery failure", async () => {
    const disk = storage();
    const calls: string[] = [];
    const journal = new OfflineJournal(
      {
        ...disk,
        sendMessage: async (message: any) => {
          calls.push(message.params.message);
          throw new Error("connection unavailable");
        },
      } as unknown as DesktopAPI,
      "ordered-failure",
    );
    await journal.enqueue("work", "say", { message: "A" });
    const b = await journal.enqueue("work", "say", { message: "B" });
    await expect(journal.deliver(b)).rejects.toThrow("connection unavailable");
    expect(calls).toEqual(["A"]);
    expect(journal.snapshot.outbox.map((item) => item.state)).toEqual([
      "queued",
      "queued",
    ]);
  });
  it("clears the original draft atomically with enqueue and rejects pre-reset draft saves", async () => {
    const disk = storage();
    const journal = new OfflineJournal(
      disk as unknown as DesktopAPI,
      "atomic-draft",
    );
    await journal.drafts({ "work:orchestrator": "send me" });
    const epoch = journal.epoch;
    await journal.enqueue("work", "say", { message: "send me" }, "send me");
    const reopened = new OfflineJournal(
      disk as unknown as DesktopAPI,
      "atomic-draft",
    );
    await reopened.load();
    expect(reopened.snapshot.drafts["work:orchestrator"]).toBe("");
    expect(reopened.snapshot.outbox).toHaveLength(1);
    await journal.reset();
    await expect(
      journal.drafts({ "work:orchestrator": "old typing" }, epoch),
    ).rejects.toThrow("storage changed");
    expect(journal.snapshot.drafts).toEqual({});
  });
  it("recovers pending work independently from damaged history", async () => {
    const disk = storage();
    const before = new OfflineJournal(
      disk as unknown as DesktopAPI,
      "damaged-history",
    );
    await before.enqueue("work", "say", { message: "keep queued" });
    await before.drafts({ "work:orchestrator": "keep draft" });
    disk.values.set(before.key, "corrupt");
    const after = new OfflineJournal(
      disk as unknown as DesktopAPI,
      "damaged-history",
    );
    await after.load();
    expect(after.historyError).toContain(
      "Drafts and outbox are still available",
    );
    expect(after.snapshot.outbox).toHaveLength(1);
    expect(after.snapshot.drafts["work:orchestrator"]).toBe("keep draft");
    await after.clearHistory();
    expect(after.historyError).toBe("");
    expect(after.snapshot.outbox).toHaveLength(1);
  });
  it("preserves concurrent typing and escaped draft syntax on pending replies", async () => {
    const disk = storage();
    const journal = new OfflineJournal(
      {
        ...disk,
        sendMessage: async () => ({ action: "pending" }),
      } as unknown as DesktopAPI,
      "pending-typing",
    );
    const message = await journal.enqueue(
      "work",
      "say",
      { message: "/literal" },
      "//literal",
    );
    await journal.drafts({ "work:orchestrator": "new typing" });
    const result = await journal.deliver(message);
    const merged = appendDraft("new typing", result.restored_draft.text);
    expect(merged).toBe("new typing\n\n//literal");
    expect(journal.snapshot.drafts["work:orchestrator"]).toBe(merged);
    expect(appendDraft(merged, result.restored_draft.text)).toBe(merged);
  });
  it("moves a queued message to a draft before it can be edited and resent", async () => {
    const disk = storage();
    let calls = 0;
    const journal = new OfflineJournal(
      {
        ...disk,
        sendMessage: async () => {
          calls++;
          return {};
        },
      } as unknown as DesktopAPI,
      "move-draft",
    );
    const message = await journal.enqueue("work", "thread.send", {
      thread: "worker",
      prompt: "edit me",
    });
    await journal.moveToDraft(message.id);
    expect(journal.snapshot.outbox).toHaveLength(0);
    expect(journal.snapshot.drafts["work:worker"]).toBe("edit me");
    await expect(journal.deliver(message)).rejects.toThrow("removed");
    expect(calls).toBe(0);
  });
  it("marks expiry during a session and definite rejections for review", async () => {
    const disk = storage();
    const journal = new OfflineJournal(
      {
        ...disk,
        sendMessage: async () => {
          throw Object.assign(new Error("thread not found"), {
            delivery: "rejected",
          });
        },
      } as unknown as DesktopAPI,
      "rejected",
    );
    const rejected = await journal.enqueue("work", "say", {
      message: "rejected",
    });
    await expect(journal.deliver(rejected)).rejects.toThrow("thread not found");
    expect(journal.snapshot.outbox[0].state).toBe("review");
    const expired = await journal.enqueue("work", "say", {
      message: "expired",
    });
    expired.createdAt = Date.now() - RECEIPT_WINDOW - 1;
    await expect(journal.deliver(expired)).rejects.toThrow(
      "Check this message",
    );
    expect(journal.snapshot.outbox[1].state).toBe("review");
  });
  it("does not resurrect a draft from a delivery that finishes after reset", async () => {
    const disk = storage();
    let finish!: (value: any) => void;
    const journal = new OfflineJournal(
      {
        ...disk,
        sendMessage: () =>
          new Promise((resolve) => {
            finish = resolve;
          }),
      } as unknown as DesktopAPI,
      "reset-inflight",
    );
    const message = await journal.enqueue("work", "say", { message: "late" });
    const delivery = journal.deliver(message);
    const rejected = expect(delivery).rejects.toThrow("storage changed");
    await new Promise((resolve) => setTimeout(resolve, 0));
    await journal.reset();
    finish({ action: "pending" });
    await rejected;
    expect(journal.snapshot.drafts).toEqual({});
    expect(journal.snapshot.outbox).toEqual([]);
    expect([...disk.values.values()].join()).not.toContain("late");
  });
  it("uses one write queue when returning to a hub through a new transport", async () => {
    const disk = storage();
    const a = offlineJournal(disk as unknown as DesktopAPI, "stable-hub");
    await a.drafts({ "work:orchestrator": "initial" });
    const b = offlineJournal(
      { ...disk } as unknown as DesktopAPI,
      "stable-hub",
    );
    expect(a).toBe(b);
    await Promise.all([
      a.enqueue("work", "say", { message: "old delivery" }),
      b.drafts({ "work:orchestrator": "new typing" }),
    ]);
    const restored = new OfflineJournal(
      disk as unknown as DesktopAPI,
      "stable-hub",
    );
    await restored.load();
    expect(restored.snapshot.outbox).toHaveLength(1);
    expect(restored.snapshot.drafts["work:orchestrator"]).toBe("new typing");
  });
  it("never carries cached permission visibility into saved messages", async () => {
    const disk = storage();
    let delivered: any;
    const host = {
      ...disk,
      sendMessage: async (message: any) => {
        delivered = message;
        return { action: "pending" };
      },
    } as unknown as DesktopAPI;
    const journal = new OfflineJournal(host, "visibility-test");
    await journal.load();
    const message = await journal.enqueue("work", "say", {
      message: "yes",
      seen_gseq: 900,
      focused: "worker",
      to: "orchestrator",
    });
    expect(message.params).toEqual({ message: "yes", to: "orchestrator" });
    const result = await journal.deliver(message);
    expect(delivered.params.seen_gseq).toBeUndefined();
    expect(result.action).toBe("pending");
    expect(journal.snapshot.outbox).toHaveLength(0);
    expect(journal.snapshot.drafts["work:orchestrator"]).toBe("yes");
  });
  it("does not send a queued item removed while an earlier delivery is pending", async () => {
    const disk = storage();
    let finish!: (value: any) => void;
    const calls: string[] = [];
    const host = {
      ...disk,
      sendMessage: async (message: any) => {
        calls.push(message.id);
        if (calls.length === 1)
          return new Promise((resolve) => {
            finish = resolve;
          });
        return { ok: true };
      },
    } as unknown as DesktopAPI;
    const journal = new OfflineJournal(host, "cancel-test");
    await journal.load();
    const a = await journal.enqueue("work", "say", { message: "A" });
    const b = await journal.enqueue("work", "say", { message: "B" });
    const first = journal.deliver(a);
    const second = journal.deliver(b);
    const rejected = expect(second).rejects.toThrow("removed");
    await new Promise((resolve) => setTimeout(resolve, 0));
    await journal.remove(b.id);
    finish({ ok: true });
    await first;
    await rejected;
    expect(calls).toEqual([a.id]);
  });
  it("retries a locked store, keeps typing writes small, and prevents forgotten data returning", async () => {
    const disk = storage();
    let locked = true;
    let rejectSend!: (error: Error) => void;
    const host = {
      ...disk,
      secureRead: async (key: string) => {
        if (locked) throw new Error("locked");
        return disk.secureRead(key);
      },
      sendMessage: () =>
        new Promise((_resolve, reject) => {
          rejectSend = reject;
        }),
    } as unknown as DesktopAPI;
    const journal = new OfflineJournal(host, "forget-test");
    await expect(journal.load()).rejects.toThrow("locked");
    locked = false;
    await journal.load();
    await journal.cache("work", {
      threads: [],
      attention: [],
      decisions: [],
      decisions_seen: 0,
      head: 1,
    });
    const saved = disk.values.get("offline:forget-test");
    await journal.drafts({ "work:orchestrator": "private" });
    expect(disk.values.get("offline:forget-test")).toBe(saved);
    expect(disk.values.get("offline-pending:forget-test")).toContain("private");
    const message = await journal.enqueue("work", "say", {
      message: "inflight",
    });
    const sending = journal.deliver(message);
    const rejected = expect(sending).rejects.toThrow();
    await new Promise((resolve) => setTimeout(resolve, 0));
    await forgetOfflineHost("forget-test", host);
    rejectSend(new Error("socket closed"));
    await rejected;
    expect([...disk.values.keys()]).toEqual([]);
  });

  it("survives app termination and retries only the original message ID and workspace", async () => {
    const disk = storage();
    let sent = 0;
    const receipts = new Map<string, unknown>();
    const host = {
      ...disk,
      async sendMessage(message: any) {
        if (!receipts.has(message.id)) {
          sent++;
          receipts.set(message.id, { ok: true });
        }
        return receipts.get(message.id);
      },
    } as unknown as DesktopAPI;
    const before = new OfflineJournal(host, "phone-hub");
    await before.load();
    await before.drafts({ "work:orchestrator": "remember this" });
    const message = await before.enqueue("work", "thread.send", {
      thread: "worker-1",
      prompt: "do this once",
    });
    // Hub accepted a message whose response never reached the terminated app.
    await host.sendMessage!(message);
    const after = new OfflineJournal(host, "phone-hub");
    await after.load();
    expect(after.snapshot.drafts["work:orchestrator"]).toBe("remember this");
    expect(after.snapshot.outbox[0].id).toBe(message.id);
    expect(after.snapshot.outbox[0].workspace).toBe("work");
    await after.deliver(after.snapshot.outbox[0]);
    expect(sent).toBe(1);
    expect(after.snapshot.outbox).toHaveLength(0);
    expect(
      new OfflineJournal(host, "another-hub").snapshot.outbox,
    ).toHaveLength(0);
  });
  it("keeps expired or uncertain deliveries for review and never replays approvals", async () => {
    const id = "fd6de15a-a327-4cdf-9757-e34dc8d23325";
    const record = {
      version: 1,
      drafts: {},
      workspaces: {},
      outbox: [
        {
          id,
          workspace: "default",
          method: "say",
          params: { message: "old" },
          createdAt: Date.now() - RECEIPT_WINDOW - 1,
          state: "sending",
        },
        { id: "approval", method: "attention.respond", createdAt: Date.now() },
      ],
    };
    const parsed = parseOffline(JSON.stringify(record));
    expect(parsed.outbox).toHaveLength(1);
    expect(parsed.outbox[0].state).toBe("review");
    const disk = storage();
    await disk.secureWrite("offline:hub", JSON.stringify(record));
    let called = false;
    const journal = new OfflineJournal(
      {
        ...disk,
        sendMessage: async () => {
          called = true;
        },
      } as unknown as DesktopAPI,
      "hub",
    );
    await journal.load();
    await expect(journal.deliver(journal.snapshot.outbox[0])).rejects.toThrow(
      "Check this message",
    );
    expect(called).toBe(false);
  });
  it("serializes cache and draft writes without dropping queued messages", async () => {
    const disk = storage();
    const journal = new OfflineJournal(disk as unknown as DesktopAPI, "hub");
    await journal.load();
    await Promise.all([
      journal.enqueue("default", "say", { message: "queued" }),
      journal.drafts({ "default:orchestrator": "draft" }),
      journal.cache("default", {
        threads: [],
        attention: [],
        decisions: [],
        decisions_seen: 0,
        head: 4,
      }),
    ]);
    const restored = new OfflineJournal(disk as unknown as DesktopAPI, "hub");
    await restored.load();
    expect(restored.snapshot.outbox).toHaveLength(1);
    expect(restored.snapshot.drafts["default:orchestrator"]).toBe("draft");
    expect(restored.snapshot.workspaces.default.view.head).toBe(4);
    await restored.clear();
    expect(disk.values.has("offline:hub")).toBe(false);
  });
});
describe("desktop message receipts", () => {
  it("does not repeat a hub send after a companion crash or a lost reply", async () => {
    const disk = storage();
    const vault = { read: disk.secureRead, write: disk.secureWrite };
    const message = {
      id: "fd6de15a-a327-4cdf-9757-e34dc8d23325",
      workspace: "default",
      method: "say" as const,
      params: { message: "once" },
      createdAt: Date.now(),
    };
    let calls = 0;
    const first = new MessageReceipts(vault);
    await expect(
      first.send(message, async () => {
        calls++;
        throw new Error("connection closed");
      }),
    ).rejects.toThrow("unknown");
    const reopened = new MessageReceipts(vault);
    await expect(
      reopened.send(message, async () => {
        calls++;
        return { ok: true };
      }),
    ).rejects.toThrow("unknown");
    expect(calls).toBe(1);
    await expect(
      reopened.send(
        { ...message, params: { message: "different" } },
        async () => ({}),
      ),
    ).rejects.toThrow("different operation");
  });
  it("returns the original result when the app restarts after acceptance", async () => {
    const disk = storage();
    const vault = { read: disk.secureRead, write: disk.secureWrite };
    const message = {
      id: "fd6de15a-a327-4cdf-9757-e34dc8d23325",
      workspace: "work",
      method: "thread.send" as const,
      params: { thread: "worker", prompt: "once" },
      createdAt: Date.now(),
    };
    let calls = 0;
    await new MessageReceipts(vault).send(message, async () => {
      calls++;
      return { queued: 7 };
    });
    expect(
      await new MessageReceipts(vault).send(message, async () => {
        calls++;
        return { queued: 8 };
      }),
    ).toEqual({ queued: 7 });
    expect(calls).toBe(1);
  });
});
