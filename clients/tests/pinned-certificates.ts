import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { certificatePin } from "../desktop/pinned-tls";

export interface PinnedIdentity {
  key: Buffer;
  cert: Buffer;
  pin: string;
}

/** P-256 self-signed certificates like the gateway's identity: made with the openssl CLI
 * when there is one, else the committed fixtures in tests/fixtures/pinned-tls. */
export function pinnedCertificates(): Record<"hub" | "other", PinnedIdentity> {
  const directory = mkdtempSync(join(tmpdir(), "twr-pinned-"));
  const identities = {} as Record<"hub" | "other", PinnedIdentity>;
  try {
    for (const name of ["hub", "other"] as const) {
      let key: Buffer;
      let cert: Buffer;
      try {
        execFileSync(
          "openssl",
          [
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "2",
            "-keyout",
            join(directory, `${name}.key`),
            "-out",
            join(directory, `${name}.crt`),
            "-subj",
            "/CN=twr hub",
            "-addext",
            "subjectAltName=DNS:twr-hub.invalid",
          ],
          { stdio: "ignore" },
        );
        key = readFileSync(join(directory, `${name}.key`));
        cert = readFileSync(join(directory, `${name}.crt`));
      } catch {
        key = readFileSync(
          new URL(`./fixtures/pinned-tls/${name}.key`, import.meta.url),
        );
        cert = readFileSync(
          new URL(`./fixtures/pinned-tls/${name}.crt`, import.meta.url),
        );
      }
      const der = Buffer.from(
        cert
          .toString()
          .replace(/-----[^-]+-----/g, "")
          .replace(/\s/g, ""),
        "base64",
      );
      identities[name] = { key, cert, pin: certificatePin(der) };
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
  return identities;
}
