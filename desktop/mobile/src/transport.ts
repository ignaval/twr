import { requestId, fileDigest } from "./crypto";
import { registerPlugin } from "@capacitor/core";
import type { PluginListenerHandle } from "@capacitor/core";
import { Clipboard } from "@capacitor/clipboard";
import { Filesystem, Directory, Encoding } from "@capacitor/filesystem";
import { Share } from "@capacitor/share";
import type { DesktopAPI, DesktopEvent, Json } from "../../src/types";

export interface Hub {
  id: string;
  name: string;
  url: string;
  scopes: string[];
  gateway_id?: string;
}
export interface NativeTransport {
  listHubs(): Promise<{ hubs: Hub[] }>;
  pair(request: {
    url: string;
    code: string;
    name: string;
    hubName: string;
  }): Promise<Hub>;
  connect(request: { id: string }): Promise<void>;
  send(request: { message: string }): Promise<void>;
  disconnect(): Promise<void>;
  removeHub(request: { id: string }): Promise<void>;
  addListener(
    event: "message",
    listener: (value: { message: Json }) => void,
  ): Promise<PluginListenerHandle>;
}
export const Transport = registerPlugin<NativeTransport>("TwrTransport");
export class RemoteHost {
  private nativeConnected = false;
  private bridgeReady = false;
  private confirmedWorkspace = false;
  private connectionTail: Promise<unknown> = Promise.resolve();
  private foreground = true;
  private stopped = false;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private retryCount = 0;
  setForeground(value: boolean) {
    this.foreground = value;
    if (!value) {
      clearTimeout(this.retryTimer);
      this.retryTimer = undefined;
    } else if (!this.nativeConnected) this.scheduleReconnect();
  }
  private scheduleReconnect() {
    if (this.stopped || !this.foreground || this.retryTimer) return;
    const delay = Math.min(30_000, 1000 * 2 ** Math.min(this.retryCount++, 5));
    this.retryTimer = setTimeout(() => {
      this.retryTimer = undefined;
      void this.resume()
        .then(() => {
          this.retryCount = 0;
        })
        .catch(() => this.scheduleReconnect());
    }, delay);
  }
  private workspace = "default";
  private reconnecting?: Promise<void>;
  private pending = new Map<
    string,
    {
      resolve(value: any): void;
      reject(error: Error): void;
      timer: ReturnType<typeof setTimeout>;
      wire: string;
      durable: boolean;
    }
  >();
  private readMethods = new Set<string>();
  private inputTails = new Map<string, Promise<unknown>>();
  private sendTail: Promise<unknown> = Promise.resolve();
  private send(message: string) {
    const sent = this.sendTail
      .catch(() => {})
      .then(() => {
        if (this.stopped) throw new Error("This hub connection was closed");
        return Transport.send({ message });
      });
    this.sendTail = sent;
    return sent;
  }
  private async openSocket() {
    let timer: ReturnType<typeof setTimeout>;
    try {
      await Promise.race([
        Transport.connect({ id: this.hub.id }),
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new Error("Hub connection timed out")),
            30000,
          );
        }),
      ]);
    } finally {
      clearTimeout(timer!);
    }
  }
  private listeners = new Set<(event: DesktopEvent) => void>();
  private listener?: PluginListenerHandle;
  constructor(
    private hub: Hub,
    private showHubs: () => void,
    private chooseDirectory: () => Promise<string | null>,
  ) {}
  async start() {
    this.listener = await Transport.addListener("message", ({ message }) => {
      if (message.type === "heartbeat") {
        void this.send(
          JSON.stringify({ id: requestId(), method: "heartbeat" }),
        ).catch(() => {});
        return;
      }
      if (message.type === "resync" || message.type === "terminal-resync") {
        this.listeners.forEach((listener) =>
          listener({
            type: "resync",
            scope: message.type === "terminal-resync" ? "terminal" : "control",
          }),
        );
        return;
      }
      if (message.type === "companion-closed") {
        this.bridgeReady = false;
        this.listeners.forEach((listener) =>
          listener({
            type: "connection",
            connected: false,
            error: "Hub companion stopped; reconnecting…",
          }),
        );
        this.scheduleReconnect();
        return;
      }
      if (message.id && this.pending.has(message.id)) {
        const request = this.pending.get(message.id)!;
        this.pending.delete(message.id);
        clearTimeout(request.timer);
        if (message.error) request.reject(new Error(message.error));
        else request.resolve(message.result);
      } else {
        if (message.type === "authenticated") this.nativeConnected = true;
        if (message.type === "transport-closed") {
          this.nativeConnected = false;
          this.rejectPending(true);
          this.scheduleReconnect();
        }
        this.listeners.forEach((listener) =>
          listener(
            message.type === "transport-closed"
              ? { ...message, type: "connection" }
              : message,
          ),
        );
      }
    });
    try {
      await this.openSocket();
      this.nativeConnected = true;
      const capabilities = await this.request("bootstrap");
      this.readMethods = new Set(capabilities.read_methods || []);
      this.workspace = capabilities.workspace || "default";
    } catch (e) {
      await this.listener.remove();
      throw e;
    }
  }
  private rejectPending(preserveDurable = false) {
    this.pending.forEach((request, id) => {
      if (preserveDurable && request.durable) return;
      this.pending.delete(id);
      clearTimeout(request.timer);
      request.reject(
        new Error(
          "Connection closed. The operation may have completed; check its state before retrying.",
        ),
      );
    });
  }
  async stop() {
    this.stopped = true;
    clearTimeout(this.retryTimer);
    this.nativeConnected = false;
    this.rejectPending();
    await this.listener?.remove();
    await Transport.disconnect();
  }
  private queueConnection<T>(job: () => Promise<T>): Promise<T> {
    const operation = this.connectionTail
      .catch(() => {})
      .then(() => {
        if (this.stopped) throw new Error("This hub connection was closed");
        return job();
      });
    this.connectionTail = operation;
    return operation;
  }
  private async connectWorkspace(workspace: string) {
    if (!this.nativeConnected) {
      await this.openSocket();
      this.nativeConnected = true;
    }
    const result = await this.request("connect", { workspace }, 75000);
    this.workspace = workspace;
    this.confirmedWorkspace = true;
    this.bridgeReady = true;
    for (const pending of this.pending.values())
      if (pending.durable) void this.send(pending.wire).catch(() => {});
    await this.request("rpc", {
      method: "notify.looking",
      params: { looking: this.foreground },
    });
    this.listeners.forEach((listener) =>
      listener({ type: "resync", scope: "terminal" }),
    );
    return result;
  }
  async resume() {
    if (this.stopped) return;
    if (!this.reconnecting)
      this.reconnecting = this.queueConnection(async () => {
        // App bootstrap selects the initial workspace. Foreground/notification
        // callbacks must not start a competing default-workspace connection.
        if (!this.confirmedWorkspace) return;
        if (this.nativeConnected) {
          try {
            await this.request("heartbeat", {}, 3000);
          } catch {
            this.nativeConnected = false;
            this.bridgeReady = false;
            this.rejectPending(true);
          }
        }
        if (!this.nativeConnected || !this.bridgeReady) {
          try {
            await this.connectWorkspace(this.workspace);
          } catch (error) {
            this.bridgeReady = false;
            this.scheduleReconnect();
            throw error;
          }
        } else
          await this.request("rpc", {
            method: "notify.looking",
            params: { looking: this.foreground },
          });
      }).finally(() => {
        this.reconnecting = undefined;
      });
    return this.reconnecting;
  }
  request(method: string, params: Json = {}, timeout = 660_000): Promise<any> {
    if (this.stopped)
      return Promise.reject(new Error("This hub connection was closed"));
    const id = requestId();
    const durable =
      ["config-save", "config-set", "terminal-open", "push-register"].includes(
        method,
      ) ||
      (method === "cli" &&
        params.args?.[0] === "ws" &&
        params.args?.[1] === "new") ||
      (method === "rpc" && !this.readMethods.has(params.method));
    const wire = JSON.stringify({
      id,
      method,
      params,
      workspace: this.workspace,
    });
    if (new TextEncoder().encode(wire).byteLength > 2 * 1024 * 1024)
      return Promise.reject(
        new Error(
          "Request exceeds 2 MiB. Attach large files instead of pasting them.",
        ),
      );
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new Error("No reply from the hub. Check its state before retrying."),
        );
      }, timeout);
      this.pending.set(id, { resolve, reject, timer, wire, durable });
      void this.send(wire).catch((error) => {
        if (durable && !this.stopped && error?.code !== "REQUEST_TOO_LARGE") {
          this.nativeConnected = false;
          this.scheduleReconnect();
          return;
        }
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      });
    });
  }
  api(): DesktopAPI {
    const request = this.request.bind(this);
    const fire = (method: string, params: Json) =>
      void request(method, params).catch((error) =>
        ["terminal-resize", "terminal-focus", "terminal-ack"].includes(method)
          ? undefined
          : this.listeners.forEach((listener) =>
              listener({ type: "error", error: String(error) }),
            ),
      );
    return {
      uploadFile: async (file, thread) => {
        if (file.size > 25 * 1024 * 1024)
          throw new Error("Files can be at most 25 MiB");
        const bytes = await file.arrayBuffer();
        const sha256 = await fileDigest(bytes);
        let name = file.name
          .normalize("NFC")
          .replace(/[^\p{L}\p{N}_. -]/gu, "_");
        while (new TextEncoder().encode(name).length > 120)
          name = Array.from(name).slice(0, -1).join("");
        const upload = await request("upload-begin", {
          thread,
          name,
          size: file.size,
          sha256,
        });
        try {
          for (
            let offset = 0;
            offset < file.size;
            offset += upload.chunk_size
          ) {
            const data = await new Promise<string>((resolve, reject) => {
              const reader = new FileReader();
              reader.onerror = () => reject(reader.error);
              reader.onload = () =>
                resolve(String(reader.result).split(",")[1]);
              reader.readAsDataURL(
                file.slice(offset, offset + upload.chunk_size),
              );
            });
            await request("upload-chunk", { id: upload.id, data });
          }
          return await request("upload-commit", { id: upload.id });
        } catch (error) {
          void request("upload-cancel", { id: upload.id }).catch(() => {});
          throw error;
        }
      },
      quit: async () => {
        if (!this.stopped) this.showHubs();
      },
      copyText: async (text) => {
        await Clipboard.write({ string: text });
      },
      bootstrap: async () => ({
        ...(await request("bootstrap")),
        platform: navigator.userAgent.includes("Android") ? "android" : "ios",
      }),
      connect: (workspace) =>
        this.queueConnection(() => this.connectWorkspace(workspace)),
      rpc: (method, params = {}) => request("rpc", { method, params }),
      cli: (args) => request("cli", { args }),
      catalog: () => request("catalog"),
      terminalOpen: (args) => request("terminal-open", args),
      terminalFocus: (focused) => fire("terminal-focus", { focused }),
      terminalAck: () => {},
      terminalWrite: (id, data) => {
        // Bound each UTF-8 chunk while preserving code points and input order.
        const previous = this.inputTails.get(id) || Promise.resolve();
        const sent = previous
          .catch(() => {})
          .then(async () => {
            for (let offset = 0; offset < data.length;) {
              let end = Math.min(offset + 8192, data.length);
              if (end < data.length && /[\uD800-\uDBFF]/.test(data[end - 1]))
                end--;
              await request("terminal-write", {
                id,
                data: data.slice(offset, end),
              });
              offset = end;
            }
          });
        this.inputTails.set(id, sent);
        void sent
          .catch((error) =>
            this.listeners.forEach((listener) =>
              listener({ type: "error", error: String(error) }),
            ),
          )
          .finally(() => {
            if (this.inputTails.get(id) === sent) this.inputTails.delete(id);
          });
      },
      terminalResize: (id, cols, rows) =>
        fire("terminal-resize", { id, cols, rows }),
      terminalClose: (id) => request("terminal-close", { id }),
      terminalRead: (id) => request("terminal-read", { id }),
      pickDirectory: () =>
        this.stopped ? Promise.resolve(null) : this.chooseDirectory(),
      openExternal: async (url) => {
        const parsed = new URL(url);
        if (!["https:", "http:", "mailto:"].includes(parsed.protocol))
          throw new Error("Unsupported link");
        const { Browser } = await import("@capacitor/browser");
        await Browser.open({ url: parsed.href });
      },
      reveal: async (path) => {
        const { Dialog } = await import("@capacitor/dialog");
        await Dialog.alert({ title: "Path on your hub", message: path });
      },
      configRead: () => request("config-read"),
      configSave: (content, hash) => request("config-save", { content, hash }),
      configSet: (table, key, value) =>
        request("config-set", { table, key, value }),
      exportText: async (name, content) => {
        if (!/^[a-zA-Z0-9_. -]{1,120}$/.test(name))
          throw new Error("Invalid export name");
        const file = await Filesystem.writeFile({
          path: name,
          directory: Directory.Cache,
          encoding: Encoding.UTF8,
          data: content,
        });
        await Share.share({ title: name, url: file.uri });
      },
      onEvent: (callback) => {
        this.listeners.add(callback);
        return () => {
          this.listeners.delete(callback);
        };
      },
    };
  }
}
