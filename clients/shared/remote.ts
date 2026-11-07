import { isOfflineHostForgotten } from "./offline";
import { RECEIPT_WINDOW } from "./offline-constants";
import { FrameAssembler } from "./remote-framing";
import { requestId } from "./remote-crypto";
import { createUploadFile, createCancelUpload } from "./uploads";
import { aggregateInboxSources } from "./inbox-cache";
import type { DesktopAPI, DesktopEvent, Json } from "./types";

export interface Hub {
  id: string;
  name: string;
  url: string;
  scopes: string[];
  gateway_id?: string;
}
export interface PairRequest {
  url: string;
  code: string;
  name: string;
  hubName: string;
}
/** Native code owns credentials, TLS and exactly one active socket. */
export interface NativeTransport {
  listHubs(): Promise<{ hubs: Hub[] }>;
  pair(request: PairRequest): Promise<Hub>;
  connect(request: { id: string }): Promise<void>;
  send(request: { message: string }): Promise<void>;
  disconnect(): Promise<void>;
  removeHub(request: { id: string }): Promise<void>;
  addListener(
    event: "message",
    listener: (value: { message: Json }) => void,
  ): Promise<{ remove(): Promise<void> }>;
}
export interface RemoteServices {
  platform: string;
  copyText: DesktopAPI["copyText"];
  openExternal: DesktopAPI["openExternal"];
  reveal: DesktopAPI["reveal"];
  exportText: DesktopAPI["exportText"];
  setAppearance?: DesktopAPI["setAppearance"];
  secureRead?: DesktopAPI["secureRead"];
  secureWrite?: DesktopAPI["secureWrite"];
  secureDelete?: DesktopAPI["secureDelete"];
  includeLocalInboxCache?: boolean;
}
export class RemoteHost {
  private inboxRefresh?: Promise<import("./types").InboxSource[]>;
  private async cachedBootstrap() {
    if (!this.services.secureRead || isOfflineHostForgotten(this.hub.id))
      return undefined;
    try {
      const cached = JSON.parse(
        (await this.services.secureRead(`bootstrap:${this.hub.id}`)) || "null",
      );
      if (cached && Date.now() - cached.at < 7 * 86400_000) return cached.value;
    } catch {}
    return undefined;
  }
  private async bootstrap() {
    if (!this.nativeConnected) {
      const cached = await this.cachedBootstrap();
      if (cached) return { ...cached, terminals: [] };
      throw new Error(
        "No saved history is available. Connect to this hub once first.",
      );
    }
    const value = await this.request("bootstrap", {}, 30_000);
    try {
      if (!isOfflineHostForgotten(this.hub.id))
        await this.services.secureWrite?.(
          `bootstrap:${this.hub.id}`,
          JSON.stringify({
            at: Date.now(),
            value: {
              workspace: value.workspace,
              scopes: value.scopes,
              allowed_methods: value.allowed_methods,
              read_methods: value.read_methods,
              home: "",
              version: value.version,
              theme: null,
              terminals: [],
            },
          }),
        );
    } catch {
      /* An unavailable cache must not break the live connection. */
    }
    return value;
  }
  private nativeConnected = false;
  private framing = false;
  private bridgeReady = false;
  private confirmedWorkspace = false;
  private connectionTail: Promise<unknown> = Promise.resolve();
  private foreground = true;
  private looking = true;
  setLooking(value: boolean) {
    this.looking = value;
    if (this.nativeConnected && this.bridgeReady && !this.stopped)
      void this.request("rpc", {
        method: "notify.looking",
        params: { looking: value },
      }).catch(() => {});
  }
  private stopped = false;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private retryCount = 0;
  private authenticationRevoked = false;
  private assembler = new FrameAssembler();
  private lastFrameAt = Date.now();
  private closing: Promise<void> = Promise.resolve();
  private transportFailed(error: string) {
    if (this.stopped) return;
    this.nativeConnected = false;
    this.bridgeReady = false;
    this.assembler.clear();
    this.rejectPending(true);
    this.listeners.forEach((listener) =>
      listener({ type: "connection", connected: false, error }),
    );
    this.closing = this.transport.disconnect().catch(() => {});
    void this.closing.then(() => this.scheduleReconnect());
  }
  private heartbeatTimer?: ReturnType<typeof setInterval>;
  private startHeartbeat() {
    if (this.heartbeatTimer) return;
    this.heartbeatTimer = setInterval(() => {
      if (this.nativeConnected && !this.stopped && this.foreground) {
        if (Date.now() - this.lastFrameAt > 45000) {
          this.transportFailed("Remote hub stopped responding; reconnecting…");
          return;
        }
        void this.send(
          JSON.stringify({ id: requestId(), method: "heartbeat" }),
        ).catch(() => {});
      }
    }, 15000);
  }
  setForeground(value: boolean) {
    this.foreground = value;
    this.looking = value;
    if (!value) {
      clearTimeout(this.retryTimer);
      this.retryTimer = undefined;
    } else if (!this.nativeConnected) this.scheduleReconnect();
  }
  private scheduleReconnect() {
    if (
      this.stopped ||
      this.authenticationRevoked ||
      !this.foreground ||
      this.retryTimer
    )
      return;
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
        return this.transport.send({ message });
      });
    this.sendTail = sent;
    return sent;
  }
  private async openSocket() {
    await this.closing;
    if (this.stopped) throw new Error("This hub connection was closed");
    let timer: ReturnType<typeof setTimeout>;
    try {
      await Promise.race([
        this.transport.connect({ id: this.hub.id }),
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new Error("Hub connection timed out")),
            30000,
          );
        }),
      ]);
    } catch (error) {
      if (/AUTH_REVOKED|credential.*revok|re-pair/i.test(String(error))) {
        this.authenticationRevoked = true;
        clearTimeout(this.retryTimer);
        this.listeners.forEach((listener) =>
          listener({
            type: "connection",
            connected: false,
            error: String(error),
          }),
        );
      }
      throw error;
    } finally {
      clearTimeout(timer!);
    }
  }
  private listeners = new Set<(event: DesktopEvent) => void>();
  private listener?: { remove(): Promise<void> };
  constructor(
    private hub: Hub,
    private showHubs: () => void,
    private chooseDirectory: () => Promise<string | null>,
    private transport: NativeTransport,
    private services: RemoteServices,
  ) {}
  async start() {
    this.listener = await this.transport.addListener(
      "message",
      ({ message: frame }) => {
        if (this.stopped) return;
        this.lastFrameAt = Date.now();
        let message: Json | undefined;
        try {
          message = this.assembler.accept(frame);
        } catch (error) {
          this.transportFailed(String(error));
          return;
        }
        if (!message) return;
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
              scope:
                message.type === "terminal-resync" ? "terminal" : "control",
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
          if (message.error)
            request.reject(
              Object.assign(new Error(message.error), { delivery: "rejected" }),
            );
          else request.resolve(message.result);
        } else {
          if (message.type === "authenticated") {
            this.nativeConnected = true;
            this.framing = message.framing === true;
          }
          if (message.type === "transport-closed") {
            this.assembler.clear();
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
      },
    );
    try {
      await this.openSocket();
      this.nativeConnected = true;
      this.startHeartbeat();
      const capabilities = await this.bootstrap();
      this.readMethods = new Set(capabilities.read_methods || []);
      this.workspace = capabilities.workspace || "default";
    } catch (e) {
      if (
        this.stopped ||
        /AUTH_REVOKED|credential.*revok|re-pair/i.test(String(e))
      )
        throw e;
      const cached = await this.cachedBootstrap();
      if (!cached) {
        await this.listener.remove();
        throw e;
      }
      this.nativeConnected = false;
      this.bridgeReady = false;
      this.workspace = cached.workspace || "default";
      this.readMethods = new Set(cached.read_methods || []);
      this.confirmedWorkspace = true;
      this.closing = this.transport.disconnect().catch(() => {});
      this.startHeartbeat();
      this.scheduleReconnect();
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
    clearInterval(this.heartbeatTimer);
    this.assembler.clear();
    clearTimeout(this.retryTimer);
    this.nativeConnected = false;
    this.rejectPending();
    await this.listener?.remove();
    await this.closing;
    await this.transport.disconnect();
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
      params: { looking: this.looking },
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
        if (this.nativeConnected && Date.now() - this.lastFrameAt >= 15000) {
          const probingSince = Date.now();
          try {
            await this.request("heartbeat", {}, 15000);
          } catch {
            // A response may be behind already-buffered TCP data. Receiving
            // fragments while probing proves progress on the existing link.
            if (!this.nativeConnected || this.lastFrameAt < probingSince) {
              this.transportFailed(
                "Remote hub stopped responding; reconnecting…",
              );
            }
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
        } else {
          try {
            await this.request("rpc", {
              method: "notify.looking",
              params: { looking: this.looking },
            });
          } catch (error) {
            this.transportFailed(String(error));
            await this.connectWorkspace(this.workspace);
          }
        }
      }).finally(() => {
        this.reconnecting = undefined;
      });
    return this.reconnecting;
  }
  request(
    method: string,
    params: Json = {},
    timeout = 660_000,
    identity?: { id: string; workspace: string },
  ): Promise<any> {
    if (this.stopped)
      return Promise.reject(new Error("This hub connection was closed"));
    const id = identity?.id || requestId();
    const durable =
      ["config-save", "config-set", "terminal-open"].includes(method) ||
      (method === "cli" &&
        params.args?.[0] === "ws" &&
        params.args?.[1] === "new") ||
      (method === "rpc" && !this.readMethods.has(params.method));
    const wire = JSON.stringify({
      id,
      method,
      params,
      workspace: identity?.workspace || this.workspace,
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
      inboxSources: () =>
        (this.inboxRefresh ||= aggregateInboxSources({
          live: async () =>
            (await request("inbox-sources")).map(
              (source: import("./types").InboxSource) => ({
                ...source,
                hubId: this.hub.id,
                hubName: this.hub.name,
              }),
            ),
          listHubs: () => this.transport.listHubs(),
          secureRead: this.services.secureRead,
          currentHub: this.hub,
          includeLocal: this.services.includeLocalInboxCache,
        }).finally(() => {
          this.inboxRefresh = undefined;
        })),
      sendMessage: (message) => {
        if (
          !["say", "thread.send"].includes(message.method) ||
          Date.now() - message.createdAt >= RECEIPT_WINDOW ||
          message.createdAt > Date.now() + 60_000
        )
          return Promise.reject(
            new Error(
              "Message is outside the safe retry window. Check its delivery before trying again.",
            ),
          );
        if (message.workspace !== this.workspace || !this.bridgeReady)
          return Promise.reject(
            new Error(
              "Connect to this message’s workspace before delivering it",
            ),
          );
        return this.request(
          "rpc",
          { method: message.method, params: message.params },
          660_000,
          { id: message.id, workspace: message.workspace },
        );
      },
      secureRead: this.services.secureRead,
      secureWrite: this.services.secureWrite,
      secureDelete: this.services.secureDelete,
      uploadFile: createUploadFile(
        (method, params) => request(method, params, 15_000),
        () => this.workspace,
      ),
      cancelUpload: createCancelUpload(
        (method, params) => request(method, params, 15_000),
        () => this.workspace,
      ),
      quit: async () => {
        if (!this.stopped) this.showHubs();
      },
      copyText: async (text) => {
        await this.services.copyText(text);
      },
      bootstrap: async () => ({
        ...(await this.bootstrap()),
        transportFraming: this.framing,
        platform: this.services.platform,
        hostId: this.hub.id,
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
        await this.services.openExternal(parsed.href);
      },
      reveal: (path) => this.services.reveal(path),
      configRead: () => request("config-read"),
      configSave: (content, hash) => request("config-save", { content, hash }),
      configSet: (table, key, value) =>
        request("config-set", { table, key, value }),
      exportText: async (name, content) => {
        if (!/^[a-zA-Z0-9_. -]{1,120}$/.test(name))
          throw new Error("Invalid export name");
        await this.services.exportText(name, content);
      },
      setAppearance: this.services.setAppearance,
      onEvent: (callback) => {
        this.listeners.add(callback);
        return () => {
          this.listeners.delete(callback);
        };
      },
    };
  }
}
