import { request as httpsRequest } from "node:https";
import { request as httpRequest } from "node:http";
import {
  checkServerIdentity,
  getCACertificates,
  type TLSSocket,
} from "node:tls";
import { isIP } from "node:net";

/** Use the OS trust store for HTTP pairing and WebSockets, with Node's default roots. */
export function systemCAs(): string[] {
  return [
    ...new Set([
      ...getCACertificates("default"),
      ...getCACertificates("system"),
    ]),
  ];
}
export interface GatewayRequest {
  method?: "GET" | "POST";
  body?: string;
  timeout?: number;
  connect?: { host: string; port: number };
  ca?: string[];
  /** A TLS socket whose key was already checked against a pin (pinned-tls.ts): the
   * request is written on it, and nothing here validates certificates. */
  socket?: TLSSocket;
}
/** Bounded JSON, no redirects, mandatory TLS validation against the public hostname. */
export function gatewayRequest(
  base: string,
  path: string,
  options: GatewayRequest = {},
): Promise<{ status: number; data: any }> {
  const url = new URL(path, base);
  const identity = url.hostname.replace(/^\[|\]$/g, "");
  return new Promise((resolve, reject) => {
    let finished = false;
    const complete = (error?: Error, value?: { status: number; data: any }) => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      // One request per pinned connection: it closes with its answer.
      options.socket?.destroy();
      if (error) reject(error);
      else resolve(value!);
    };
    const pinned = options.socket;
    const request = (
      url.protocol === "https:" && !pinned ? httpsRequest : httpRequest
    )(
      {
        ...(pinned
          ? { createConnection: () => pinned, protocol: "http:" }
          : { protocol: url.protocol }),
        hostname: options.connect?.host || identity,
        port:
          options.connect?.port ||
          Number(url.port || (url.protocol === "https:" ? 443 : 80)),
        path: `${url.pathname}${url.search}`,
        method: options.method || "GET",
        headers: {
          Host: url.host,
          Accept: "application/json",
          ...(options.body
            ? {
                "Content-Type": "application/json",
                "Content-Length": Buffer.byteLength(options.body),
              }
            : {}),
        },
        ...(url.protocol === "https:" && !pinned
          ? {
              ca: options.ca || systemCAs(),
              servername: isIP(identity) ? undefined : identity,
              checkServerIdentity: (
                _hostname: string,
                certificate: Parameters<typeof checkServerIdentity>[1],
              ) => checkServerIdentity(identity, certificate),
              minVersion: "TLSv1.2" as const,
            }
          : {}),
      },
      (response) => {
        const chunks: Buffer[] = [];
        let size = 0;
        response.on("data", (chunk: Buffer) => {
          size += chunk.length;
          if (size > 16 * 1024) {
            request.destroy();
            complete(new Error("Gateway response exceeds its limit"));
            return;
          }
          chunks.push(chunk);
        });
        response.on("error", (error) => complete(error));
        response.on("end", () => {
          try {
            complete(undefined, {
              status: response.statusCode || 0,
              data: JSON.parse(Buffer.concat(chunks).toString("utf8")),
            });
          } catch {
            // A refusal's status says enough when its body is no JSON (a bare 404).
            if (pinned && response.statusCode && response.statusCode !== 200)
              complete(undefined, { status: response.statusCode, data: {} });
            else complete(new Error("The gateway returned invalid JSON"));
          }
        });
      },
    );
    const timer = setTimeout(() => {
      request.destroy();
      complete(new Error("Gateway connection timed out"));
    }, options.timeout || 15_000);
    request.on("error", (error) => complete(error));
    if (options.body) request.write(options.body);
    request.end();
  });
}
