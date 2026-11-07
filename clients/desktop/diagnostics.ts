import {
  appendFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  statSync,
} from "node:fs";
import { join } from "node:path";

/**
 * Remove secrets and identifying paths before text reaches the log or the
 * clipboard. The log never leaves the machine unless the user copies it.
 */
export function redact(text: string, home?: string) {
  let value = text
    // Whole cookie headers: every cookie in them is a credential.
    .replace(/\b((?:set-)?cookie\s*:\s*)[^\n]*/gi, "$1[redacted]")
    // Authorization headers and bearer tokens.
    .replace(
      /\b(Bearer|Basic|token)\s+[A-Za-z0-9._~+/=-]{8,}/gi,
      "$1 [redacted]",
    )
    // key=value / "key": "value" pairs whose names imply a secret.
    .replace(
      /(["']?\b[\w-]*(?:token|secret|password|passphrase|api[_-]?key|authorization|cookie|pairing[_-]?code|private[_-]?key|credential|session[_-]?id)[\w-]*["']?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,;}&]+)/gi,
      "$1[redacted]",
    )
    // URL credentials and query secrets.
    .replace(/(\w+:\/\/)[^\s/@:]+:[^\s/@]+@/g, "$1[redacted]@")
    .replace(
      /([?&#](?:code|token|key|sig|signature|access_token|id_token|state)=)[^&#\s]+/gi,
      "$1[redacted]",
    )
    // One-use pairing invitations carry a "code" string: ten characters of
    // A-Z/2-9 (twr-core devices.rs). Error constants ("REQUEST_TOO_LARGE",
    // "ENOENT") stay readable; any ten-character code is always redacted.
    .replace(/("code"\s*:\s*)"([^"]*)"/g, (match, prefix, code) =>
      code.length !== 10 && (/_/.test(code) || /^E[A-Z]+$/.test(code))
        ? match
        : `${prefix}"[redacted]"`,
    )
    // Well-known credential shapes.
    .replace(/\bsk-[A-Za-z0-9_-]{16,}/g, "[redacted-key]")
    .replace(/\bgh[pousr]_[A-Za-z0-9]{20,}/g, "[redacted-key]")
    .replace(/\bgithub_pat_[A-Za-z0-9_]{20,}/g, "[redacted-key]")
    .replace(/\b(?:AKIA|ASIA)[A-Z0-9]{16}\b/g, "[redacted-key]")
    .replace(/\bAIza[A-Za-z0-9_-]{30,}/g, "[redacted-key]")
    .replace(
      /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+/g,
      "[redacted-jwt]",
    )
    .replace(/\bxox[abprs]-[A-Za-z0-9-]{10,}/g, "[redacted-key]")
    .replace(
      /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g,
      "[redacted-private-key]",
    );
  if (home && home.length > 1) value = value.split(home).join("~");
  return value;
}

export type LogLevel = "info" | "warn" | "error";

export class DiagnosticLog {
  readonly path: string;
  private readonly rotated: string;
  private bytes = 0;
  constructor(
    readonly directory: string,
    private readonly home: string,
    private readonly maxBytes = 2 * 1024 * 1024,
    private readonly now: () => Date = () => new Date(),
  ) {
    this.path = join(directory, "twr-desktop.log");
    this.rotated = join(directory, "twr-desktop.1.log");
    try {
      mkdirSync(directory, { recursive: true, mode: 0o700 });
      this.bytes = existsSync(this.path) ? statSync(this.path).size : 0;
    } catch {}
  }
  write(level: LogLevel, scope: string, message: string, detail?: unknown) {
    const extra =
      detail === undefined
        ? ""
        : " " +
          (detail instanceof Error
            ? detail.stack || detail.message
            : typeof detail === "string"
              ? detail
              : safeJson(detail));
    const line =
      redact(
        `${this.now().toISOString()} ${level.toUpperCase()} [${scope}] ${message}${extra}`,
        this.home,
      )
        .slice(0, 16 * 1024)
        .replace(/\r?\n/g, "\n    ") + "\n";
    try {
      if (this.bytes + line.length > this.maxBytes) {
        renameSync(this.path, this.rotated);
        this.bytes = 0;
      }
      appendFileSync(this.path, line, { mode: 0o600 });
      this.bytes += Buffer.byteLength(line);
    } catch {}
  }
  info(scope: string, message: string, detail?: unknown) {
    this.write("info", scope, message, detail);
  }
  warn(scope: string, message: string, detail?: unknown) {
    this.write("warn", scope, message, detail);
  }
  error(scope: string, message: string, detail?: unknown) {
    this.write("error", scope, message, detail);
  }
  /** The newest lines across the current and rotated log. */
  tail(lines = 200) {
    const read = (path: string) => {
      try {
        return readFileSync(path, "utf8");
      } catch {
        return "";
      }
    };
    const all = (read(this.rotated) + read(this.path))
      .split("\n")
      .filter(Boolean);
    return all.slice(-lines).join("\n");
  }
}

function safeJson(value: unknown) {
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

/** Accept bounded renderer reports only; a broken page must not flood the log. */
export class RateLimiter {
  private stamps: number[] = [];
  constructor(
    private readonly limit: number,
    private readonly windowMs: number,
    private readonly now: () => number = Date.now,
  ) {}
  allow() {
    const time = this.now();
    this.stamps = this.stamps.filter((stamp) => time - stamp < this.windowMs);
    if (this.stamps.length >= this.limit) return false;
    this.stamps.push(time);
    return true;
  }
}

export function countCrashReports(directory: string) {
  let count = 0;
  const walk = (path: string, depth: number) => {
    if (depth > 3) return;
    let entries;
    try {
      entries = readdirSync(path, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (entry.isDirectory()) walk(join(path, entry.name), depth + 1);
      else if (entry.name.endsWith(".dmp")) count++;
    }
  };
  walk(directory, 0);
  return count;
}

export function diagnosticsReport(
  facts: Record<string, unknown>,
  log: string,
  home: string,
) {
  const lines = ["twr desktop diagnostics", ""];
  for (const [key, value] of Object.entries(facts))
    lines.push(
      `${key}: ${typeof value === "string" ? value : safeJson(value)}`,
    );
  lines.push("", "Recent log (secrets and home directory redacted):", log);
  return redact(lines.join("\n"), home);
}
