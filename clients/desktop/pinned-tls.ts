import { connect as tlsConnect, type TLSSocket } from "node:tls";
import { X509Certificate, createHash } from "node:crypto";
import {
  RouteError,
  base64UrlEncode,
  displayRoute,
  parseRoute,
} from "../shared/phone-pairing";

/** `base64url-nopad(SHA-256(SubjectPublicKeyInfo DER))` of a DER certificate. */
export function certificatePin(der: Buffer): string {
  const spki = new X509Certificate(der).publicKey.export({
    type: "spki",
    format: "der",
  });
  return base64UrlEncode(createHash("sha256").update(spki).digest());
}

function reason(error: NodeJS.ErrnoException) {
  switch (error.code) {
    case "ECONNREFUSED":
      return "unreachable (nothing is listening there)";
    case "ENOTFOUND":
    case "EAI_AGAIN":
      return "unreachable (the name doesn't resolve)";
    case "EHOSTUNREACH":
    case "ENETUNREACH":
    case "EADDRNOTAVAIL":
      return "unreachable (no route to it from this network)";
    case "ECONNRESET":
      return "unreachable (the connection was reset)";
    default:
      return error.code?.includes("TLS") || error.code?.includes("SSL")
        ? "unreachable (no TLS there)"
        : "unreachable";
  }
}

/**
 * Opens TLS to a `tls:` route and checks the leaf certificate's SPKI SHA-256 against
 * `expected` on `secureConnect`, before the caller writes anything: not the request line,
 * not a code, not a token. Chain, name and validity dates are ignored for a pinned hub
 * (docs/design/phone-pairing.md › Client trust rule); a mismatch destroys the socket and
 * rejects with PIN_MISMATCH. With `expected` undefined (typed pairing, step 1) the pin is
 * only captured, for the proof to vouch for.
 *
 * Node calls `checkServerIdentity` only after chain verification succeeds, so a
 * self-signed key can't be checked there: `rejectUnauthorized: false` plus this check,
 * with the verified socket handed to `http` or `ws` through `createConnection`.
 */
export function pinnedConnect(
  route: string,
  expected: string | undefined,
  options: { signal?: AbortSignal; timeout?: number; hubName?: string } = {},
): Promise<{ socket: TLSSocket; pin: string }> {
  const parsed = parseRoute(route);
  if (parsed.kind !== "tls")
    return Promise.reject(new RouteError(route, "not a route this app dials"));
  return new Promise((resolve, reject) => {
    let settled = false;
    const socket = tlsConnect({
      host: parsed.host,
      port: parsed.port,
      ...(parsed.host.includes(":") || /^[\d.]+$/.test(parsed.host)
        ? {}
        : { servername: parsed.host }),
      rejectUnauthorized: false,
      minVersion: "TLSv1.2",
      ALPNProtocols: ["http/1.1"],
    });
    const fail = (error: Error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", abort);
      socket.destroy();
      reject(error);
    };
    const abort = () =>
      fail(new RouteError(route, "cancelled: another route answered first"));
    const timer = setTimeout(
      () => fail(new RouteError(route, "timed out")),
      options.timeout ?? 10_000,
    );
    if (options.signal?.aborted) return abort();
    options.signal?.addEventListener("abort", abort);
    const onError = (error: NodeJS.ErrnoException) =>
      fail(new RouteError(route, reason(error)));
    const onClose = () =>
      fail(new RouteError(route, "unreachable (closed during TLS)"));
    socket.on("error", onError);
    socket.once("close", onClose);
    socket.once("secureConnect", () => {
      let pin: string;
      try {
        const raw = socket.getPeerCertificate(false)?.raw;
        if (!raw) throw new Error("no certificate");
        pin = certificatePin(raw);
      } catch {
        fail(new RouteError(route, "presented no usable key"));
        return;
      }
      if (expected !== undefined && pin !== expected) {
        fail(
          new RouteError(
            route,
            `this is another device, not ${options.hubName || "your hub"}`,
            true,
          ),
        );
        return;
      }
      if (settled) {
        socket.destroy();
        return;
      }
      settled = true;
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", abort);
      socket.removeListener("close", onClose);
      socket.removeListener("error", onError);
      // An error after this point belongs to the request the caller writes.
      socket.on("error", () => {});
      resolve({ socket, pin });
    });
  });
}

/** The host:port a pinned route shows in messages. */
export { displayRoute };
