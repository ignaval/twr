import { X509Certificate, createPrivateKey, createHash } from "node:crypto";
import {
  readFileSync,
  openSync,
  closeSync,
  readSync,
  fstatSync,
  realpathSync,
  constants,
  renameSync,
  statSync,
} from "node:fs";
import { isAbsolute, resolve } from "node:path";
import { isIP } from "node:net";
import { spawn, execFile } from "node:child_process";
import { z } from "zod";
import type {
  GatewaySettings,
  GatewayCheck,
  Invitation,
  GatewayStatus,
} from "../shared/connections";
import { gatewayUrl } from "./remote-transport";
import { gatewayRequest } from "./gateway-network";
const settingsSchema = z
  .object({
    url: z.string().max(2048),
    listen: z.string().regex(/^(?:\[[0-9a-fA-F:]+\]|[0-9.]+):[0-9]{1,5}$/),
    cert: z.string().min(1).max(4096),
    key: z.string().min(1).max(4096),
    scopes: z.enum(["read", "operate", "terminal", "admin"]),
  })
  .strict();
const infoSchema = z.discriminatedUnion("running", [
  z.object({ running: z.literal(false), gateway_id: z.string() }),
  z.object({
    running: z.literal(true),
    gateway_id: z.string().min(1),
    pid: z.number().int().positive(),
    instance: z.uuid(),
    version: z.string(),
    executable: z.string(),
    listen: z.string(),
    tls_cert: z.string(),
    tls_key: z.string(),
    tls_cert_source: z.string().optional(),
    tls_key_source: z.string().optional(),
    renewal_state: z.string().optional(),
    renewal_error: z.string().nullable().optional(),
    certificate_sha256: z.string(),
    started_at: z.number(),
  }),
]);
type GatewayInfo = z.infer<typeof infoSchema>;
interface Launch {
  pid?: number;
  stop?(): Promise<void>;
  exitCode(): number | null;
  signal(): string | null;
}
export interface GatewayDependencies {
  command(args: string[], input?: string, binary?: string): Promise<any>;
  launch(settings: GatewaySettings, binary?: string): Promise<Launch>;
  local(settings: GatewaySettings): Promise<GatewayCheck>;
  pause(): Promise<void>;
}
function pem(path: string): Buffer {
  const file = openSync(path, "r");
  try {
    if (fstatSync(file).size > 1024 * 1024)
      throw new Error("TLS files must be at most 1 MiB");
    return readFileSync(file);
  } finally {
    closeSync(file);
  }
}
export function validateGatewaySettings(input: unknown): GatewaySettings {
  const settings = settingsSchema.parse(input);
  settings.url = gatewayUrl(settings.url);
  if (!isAbsolute(settings.cert) || !isAbsolute(settings.key))
    throw new Error(
      "Use absolute paths for the TLS certificate and private key",
    );
  const address = settings.listen.slice(0, settings.listen.lastIndexOf(":"));
  const port = Number(
    settings.listen.slice(settings.listen.lastIndexOf(":") + 1),
  );
  if (!isIP(address.replace(/^\[|\]$/g, "")) || port < 1 || port > 65535)
    throw new Error("Enter an IP address and a port from 1 to 65535");
  const certificate = new X509Certificate(pem(settings.cert));
  const key = createPrivateKey(pem(settings.key));
  const host = new URL(settings.url).hostname.replace(/^\[|\]$/g, "");
  if (isIP(host) ? !certificate.checkIP(host) : !certificate.checkHost(host))
    throw new Error("The TLS certificate does not cover the gateway hostname");
  if (
    Date.parse(certificate.validFrom) > Date.now() ||
    Date.parse(certificate.validTo) <= Date.now()
  )
    throw new Error("The TLS certificate is expired or not yet valid");
  if (!certificate.checkPrivateKey(key))
    throw new Error("The TLS private key does not match the certificate");
  return settings;
}
export function certificateDigest(path: string): string {
  return createHash("sha256").update(pem(path)).digest("hex");
}
export async function checkLocalGateway(
  settings: GatewaySettings,
): Promise<GatewayCheck> {
  const address = settings.listen
    .slice(0, settings.listen.lastIndexOf(":"))
    .replace(/^\[|\]$/g, "");
  const host =
    address === "0.0.0.0" ? "127.0.0.1" : address === "::" ? "::1" : address;
  const port = Number(
    settings.listen.slice(settings.listen.lastIndexOf(":") + 1),
  );
  const { status, data } = await gatewayRequest(settings.url, "/health", {
    timeout: 2000,
    connect: { host, port },
  });
  if (
    status !== 200 ||
    data.service !== "twr-mobile" ||
    data.protocol !== 1 ||
    typeof data.gateway_id !== "string" ||
    !data.gateway_id
  )
    throw new Error(
      "The local listener did not identify itself as a twr gateway",
    );
  return { url: settings.url, gatewayId: data.gateway_id, tls: true };
}
export class GatewaySetup {
  private settings?: GatewaySettings;
  private activeCode?: string;
  private tail: Promise<unknown> = Promise.resolve();
  private dependencies: GatewayDependencies;
  constructor(
    private binary: string,
    private environment: () => NodeJS.ProcessEnv,
    private logPath: string,
    private check: (url: string, timeout?: number) => Promise<GatewayCheck>,
    private save: (settings: GatewaySettings) => void,
    saved?: unknown,
    dependencies?: Partial<GatewayDependencies>,
  ) {
    const parsed = settingsSchema.safeParse(saved);
    if (parsed.success) this.settings = parsed.data;
    this.dependencies = {
      command: (args, input, binary = this.binary) =>
        new Promise((resolve, reject) => {
          const child = execFile(
            binary,
            args,
            { env: this.environment(), timeout: 10_000, maxBuffer: 64 * 1024 },
            (error, stdout) => {
              if (error) {
                reject(error);
                return;
              }
              try {
                resolve(JSON.parse(stdout));
              } catch (error) {
                reject(error);
              }
            },
          );
          child.stdin?.end(input || "");
        }),
      local: checkLocalGateway,
      pause: () => new Promise((resolve) => setTimeout(resolve, 250)),
      launch: async (settings, binary = this.binary) => {
        try {
          if (statSync(this.logPath).size > 512 * 1024)
            renameSync(this.logPath, `${this.logPath}.1`);
        } catch {}
        const log = openSync(
          this.logPath,
          constants.O_WRONLY |
            constants.O_CREAT |
            constants.O_APPEND |
            constants.O_NOFOLLOW,
          0o600,
        );
        try {
          const child = spawn(
            binary,
            [
              "--log-file",
              this.logPath,
              "--listen",
              settings.listen,
              "--tls-cert",
              settings.cert,
              "--tls-key",
              settings.key,
            ],
            {
              env: this.environment(),
              detached: true,
              stdio: ["ignore", log, log],
            },
          );
          await new Promise<void>((resolve, reject) => {
            child.once("error", reject);
            child.once("spawn", resolve);
          });
          child.unref();
          return {
            pid: child.pid,
            stop: async () => {
              if (child.exitCode !== null || child.signalCode) return;
              child.kill("SIGTERM");
              await new Promise<void>((resolve) => {
                const timer = setTimeout(() => {
                  if (child.exitCode === null && !child.signalCode)
                    child.kill("SIGKILL");
                  resolve();
                }, 3000);
                child.once("exit", () => {
                  clearTimeout(timer);
                  resolve();
                });
              });
            },
            exitCode: () => child.exitCode,
            signal: () => child.signalCode,
          };
        } finally {
          closeSync(log);
        }
      },
      ...dependencies,
    };
  }
  saved(): GatewaySettings | null {
    return this.settings ? { ...this.settings } : null;
  }
  private serial<T>(job: () => Promise<T>): Promise<T> {
    const result = this.tail.catch(() => {}).then(job);
    this.tail = result;
    return result;
  }
  private async info(): Promise<GatewayInfo> {
    return infoSchema.parse(
      await this.dependencies.command(["--gateway-info"]),
    );
  }
  private differs(
    settings: GatewaySettings,
    info: Extract<GatewayInfo, { running: true }>,
    binary = this.binary,
  ): boolean {
    return (
      info.listen !== settings.listen ||
      info.executable !== realpathSync(binary) ||
      (info.tls_cert_source
        ? info.tls_cert_source !== resolve(settings.cert)
        : info.tls_cert !== realpathSync(settings.cert)) ||
      (info.tls_key_source
        ? info.tls_key_source !== resolve(settings.key)
        : info.tls_key !== realpathSync(settings.key))
    );
  }
  private logTail(): string {
    try {
      const fd = openSync(
        this.logPath,
        constants.O_RDONLY | constants.O_NOFOLLOW,
      );
      try {
        const size = fstatSync(fd).size;
        const data = Buffer.alloc(Math.min(4096, size));
        readSync(fd, data, 0, data.length, Math.max(0, size - data.length));
        return data
          .toString("utf8")
          .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "")
          .trim();
      } finally {
        closeSync(fd);
      }
    } catch {
      return "";
    }
  }
  private childFailure(child: Launch): Error | undefined {
    const code = child.exitCode();
    const signal = child.signal();
    if (code === null && !signal) return;
    const tail = this.logTail();
    return new Error(
      `The gateway exited before becoming ready (${signal || `status ${code}`}).${tail ? `\n${tail}` : " Check the gateway log in the app data directory."}`,
    );
  }
  setup(input: unknown): Promise<GatewayCheck> {
    return this.serial(() => this.setupNow(validateGatewaySettings(input)));
  }
  status(): Promise<GatewayStatus> {
    return this.serial(async () => {
      const info = await this.info();
      if (!info.running) return { running: false };
      let restartRequired = false;
      try {
        if (this.settings) restartRequired = this.differs(this.settings, info);
      } catch {
        restartRequired = true;
      }
      return {
        running: true,
        runningVersion: info.version,
        listen: info.listen,
        restartRequired,
        renewalState: info.renewal_state || "current",
        renewalError: info.renewal_error || undefined,
        ...(info.renewal_state === "failed" ? { logTail: this.logTail() } : {}),
      };
    });
  }
  private async cancelNow(): Promise<void> {
    if (!this.activeCode) return;
    const code = this.activeCode;
    await this.dependencies.command(
      ["--cancel-invitation"],
      JSON.stringify({ code }),
    );
    if (this.activeCode === code) this.activeCode = undefined;
  }
  cancelInvitation(): Promise<void> {
    return this.serial(() => this.cancelNow());
  }
  stop(): Promise<void> {
    return this.serial(async () => {
      const current = await this.info();
      if (!current.running) return;
      await this.cancelNow().catch(() => {});
      await this.dependencies.command([
        "--stop-gateway",
        "--expected-instance",
        current.instance,
      ]);
      this.activeCode = undefined;
    });
  }
  restart(input: unknown): Promise<GatewayCheck> {
    return this.serial(async () => {
      const settings = validateGatewaySettings(input);
      const current = await this.info();
      if (!current.running) return this.setupNow(settings);
      // Preserve a configuration that is known to reproduce the running TLS context.
      // Refuse destructive restart if certificate renewal has removed those bytes.
      let previous: GatewaySettings;
      try {
        previous = validateGatewaySettings({
          ...(this.settings || settings),
          listen: current.listen,
          cert: current.tls_cert_source || current.tls_cert,
          key: current.tls_key_source || current.tls_key,
        });
      } catch (error) {
        throw new Error(
          `The running gateway's saved certificate cannot be used for rollback: ${error instanceof Error ? error.message : error} Use Stop gateway, then start it with the replacement certificate. This keeps the hub running.`,
        );
      }
      if (certificateDigest(previous.cert) !== current.certificate_sha256)
        throw new Error(
          "The running gateway's certificate files differ from its active TLS context. Check certificate renewal status and logs. If renewal cannot complete, use Stop gateway, then start with the replacement certificate; the hub keeps running.",
        );
      const previousBinary = realpathSync(current.executable);
      await this.cancelNow();
      await this.dependencies.command([
        "--stop-gateway",
        "--expected-instance",
        current.instance,
      ]);
      try {
        return await this.setupNow(settings);
      } catch (error) {
        try {
          const restored = await this.setupNow(previous, previousBinary);
          if (!restored.tls || restored.restartRequired)
            throw new Error(
              "Another gateway took the listener before the previous configuration could be restored",
            );
        } catch (restoreError) {
          throw new Error(
            `${error instanceof Error ? error.message : error} The previous gateway could not be restored: ${restoreError instanceof Error ? restoreError.message : restoreError}`,
          );
        }
        throw new Error(
          `${error instanceof Error ? error.message : error} The previous gateway configuration was restored.`,
        );
      }
    });
  }
  private async setupNow(
    settings: GatewaySettings,
    binary = this.binary,
  ): Promise<GatewayCheck> {
    let info = await this.info();
    if (info.running && this.differs(settings, info, binary))
      return {
        url: settings.url,
        gatewayId: info.gateway_id,
        tls: false,
        restartRequired: true,
        runningVersion: info.version,
        advisory:
          "This gateway is running with another engine, listener or TLS configuration. Restart it to apply these settings. Restarting closes gateway terminal sessions; the independent hub keeps running.",
      };
    let child: Launch | undefined;
    if (!info.running)
      child =
        binary === this.binary
          ? await this.dependencies.launch(settings)
          : await this.dependencies.launch(settings, binary);
    try {
      let health: GatewayCheck | undefined;
      let lastError: unknown;
      const deadline = Date.now() + 20_000;
      for (let attempt = 0; attempt < 30 && Date.now() < deadline; attempt++) {
        if (child) {
          const failure = this.childFailure(child);
          if (failure) throw failure;
        }
        try {
          info = await this.info();
          if (info.running) {
            if (child?.pid && info.pid !== child.pid)
              throw new Error(
                "Another gateway acquired the listener during startup",
              );
            const candidate = await this.dependencies.local(settings);
            if (candidate.gatewayId !== info.gateway_id)
              throw new Error("The local listener belongs to another gateway");
            const confirmed = await this.info();
            if (!confirmed.running || confirmed.instance !== info.instance)
              throw new Error(
                "The gateway instance changed during its TLS check",
              );
            if (child) {
              const failure = this.childFailure(child);
              if (failure) throw failure;
            }
            health = candidate;
            break;
          }
        } catch (error) {
          lastError = error;
          const code = (error as NodeJS.ErrnoException)?.code || "";
          if (/CERT|TLS|SSL|SELF_SIGNED|ISSUER|VERIFY/.test(code)) break;
        }
        if (!child) break;
        await this.dependencies.pause();
      }
      if (!health) {
        if (child) {
          const failure = this.childFailure(child);
          if (failure) throw failure;
        }
        throw new Error(
          `The local gateway could not be verified with a trusted TLS certificate for this hostname.${lastError ? ` ${lastError instanceof Error ? lastError.message : String(lastError)}` : ""} Check its listener, OS certificate trust and gateway log.`,
        );
      }
      const renewal =
        info.running &&
        info.certificate_sha256 !== certificateDigest(settings.cert)
          ? `Certificate renewal ${info.renewal_state === "failed" ? "failed" : "is pending"}; the previous verified TLS context remains active.${info.renewal_error ? ` ${info.renewal_error}` : ""}${info.renewal_state === "failed" && this.logTail() ? `\n${this.logTail()}` : ""}`
          : undefined;
      this.settings = settings;
      this.save(settings);
      try {
        const external = await this.check(settings.url, 2000);
        return {
          ...health,
          externalReachable: external.gatewayId === health.gatewayId,
          ...(renewal ? { advisory: renewal } : {}),
          ...(external.gatewayId !== health.gatewayId
            ? {
                advisory:
                  "This public address currently reaches another gateway. Correct DNS or forwarding before pairing another device.",
              }
            : {}),
        };
      } catch {
        return {
          ...health,
          externalReachable: false,
          advisory:
            renewal ||
            "Local TLS and gateway identity checks passed. This computer cannot reach the public route; this can be normal with NAT loopback. Check the address from your other device before pairing.",
        };
      }
    } catch (error) {
      // A failed check must never leave a newly launched, unverified gateway behind.
      await child?.stop?.();
      throw error;
    }
  }
  invite(input: { scopes: GatewaySettings["scopes"] }): Promise<Invitation> {
    return this.serial(async () => {
      if (!this.settings) throw new Error("Complete gateway setup first");
      const scopes = settingsSchema.shape.scopes.parse(input.scopes);
      const info = await this.info();
      if (!info.running || this.differs(this.settings, info))
        throw new Error(
          "Check or restart the local gateway before creating an invitation",
        );
      const health = await this.dependencies.local(this.settings);
      if (health.gatewayId !== info.gateway_id)
        throw new Error("The local listener belongs to another gateway");
      await this.cancelNow();
      const args = [
        "--pair",
        "--pair-only",
        "--json",
        ...(scopes === "read"
          ? ["--read-only"]
          : scopes === "terminal"
            ? ["--allow-terminal"]
            : scopes === "admin"
              ? ["--allow-terminal", "--allow-admin"]
              : []),
      ];
      const invitation = z
        .object({
          code: z.string().min(4).max(128),
          expires_at: z.number().int().positive(),
          gateway_id: z.string().min(1),
          scopes: z.array(z.string()).optional(),
        })
        .parse(await this.dependencies.command(args));
      this.activeCode = invitation.code;
      if (invitation.gateway_id !== info.gateway_id)
        throw new Error(
          "Gateway identity changed while creating the invitation",
        );
      this.settings = { ...this.settings, scopes };
      this.save(this.settings);
      return {
        url: this.settings.url,
        code: invitation.code,
        expiresAt: invitation.expires_at * 1000,
        scopes:
          invitation.scopes ||
          (scopes === "read"
            ? ["read"]
            : scopes === "operate"
              ? ["read", "operate"]
              : scopes === "terminal"
                ? ["read", "operate", "terminal"]
                : ["read", "operate", "terminal", "admin"]),
      };
    });
  }
}
