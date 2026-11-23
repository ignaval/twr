/**
 * What the pairing forms (mobile's Pair a hub, the desktop's Connect to another computer)
 * send, built from a scanned or pasted invitation or from a typed address and code.
 */
import type { PairRequest } from "./remote";
import {
  displayCode,
  displayRoute,
  normalizeCode,
  parseAddress,
  parseRoute,
  type Invitation,
} from "./phone-pairing";
import { errorMessage } from "./errors";

/** Most characters of a hub name and a device name; a certificate-mode code's bounds. */
export const PAIR_LIMITS = { codeMin: 4, code: 128, name: 80 };

/** A typed hub address that names an HTTPS URL: certificate mode. Anything else is a
 * host or IP address for pinned, typed pairing. */
export function isCertificateAddress(address: string) {
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(address.trim());
}

/** A name for the hub at an address: its host's first label ("workstation" for
 * `https://workstation.example:47321` or `workstation.local`), an IP address whole. */
export function hubNameFor(address: string): string {
  let host = "";
  const text = address.trim();
  if (isCertificateAddress(text)) {
    try {
      host = new URL(text).hostname.replace(/^\[|\]$/g, "");
    } catch {
      return "";
    }
  } else host = parseAddress(text)?.host || "";
  if (!host) return "";
  if (/^[\d.]+$/.test(host) || host.includes(":")) return host;
  return host.split(".")[0] || host;
}

/** The hub name an invitation suggests. */
export function invitationHubName(invitation: Invitation): string {
  if (invitation.version === 2) {
    if (invitation.name.trim()) return invitation.name.trim().slice(0, 80);
    const tls = invitation.routes.find((route) => route.startsWith("tls:"));
    return tls ? hubNameFor(displayRoute(tls)) : "twr hub";
  }
  return hubNameFor(invitation.url);
}

/** Where an invitation points, for its summary: a URL, or its routes. */
export function invitationAddresses(invitation: Invitation): string[] {
  if (invitation.version !== 2) return [invitation.url];
  return invitation.routes.filter((route) => {
    try {
      return parseRoute(route).kind === "tls";
    } catch {
      return false;
    }
  });
}

/** The code as the form shows it: `XXXX-XXXX-XXXX` for a twelve-character code. */
export function invitationCode(invitation: Invitation): string {
  return invitation.version === 2
    ? displayCode(invitation.code)
    : invitation.code;
}

export function requestFromInvitation(
  invitation: Invitation,
  names: { name: string; hubName: string },
): PairRequest {
  if (invitation.version === 2)
    return {
      pin: invitation.pin,
      gatewayId: invitation.id,
      routes: invitation.routes,
      ...(invitation.iroh ? { iroh: invitation.iroh } : {}),
      code: invitation.code,
      name: names.name.trim(),
      hubName: names.hubName.trim(),
    };
  return {
    url: invitation.url,
    code: invitation.code.trim(),
    name: names.name.trim(),
    hubName: names.hubName.trim(),
  };
}

/** What is wrong with a typed address and code, in words, or nothing. */
export function addressProblem(address: string, code: string) {
  const text = address.trim();
  if (isCertificateAddress(text)) {
    let url: URL | undefined;
    try {
      url = new URL(text);
    } catch {
      url = undefined;
    }
    if (!url || !/^https?:$/.test(url.protocol))
      return "The hub address isn't a web address. Enter it as the invitation shows it, such as https://workstation.example:47321.";
    const trimmed = code.trim();
    if (trimmed.length < PAIR_LIMITS.codeMin)
      return "The one-use code is too short.";
    if (trimmed.length > PAIR_LIMITS.code)
      return "The one-use code is too long.";
    return undefined;
  }
  if (!parseAddress(text))
    return "The hub address can't be used. Enter its name or IP address, with :port if it isn't 47321 (such as 192.168.1.20 or workstation.local:47321), or the https:// address of a hub with its own certificate.";
  if (!normalizeCode(code))
    return "The pairing code has 12 letters and digits, such as ABCD-EFGH-JK23. Check it on the hub.";
  return undefined;
}

/** A typed address and code: an `https://` URL pairs in certificate mode, anything else
 * by the typed pairing proof. Throws what is wrong, in words. */
export function requestFromAddress(
  address: string,
  code: string,
  names: { name: string; hubName: string },
): PairRequest {
  const problem = addressProblem(address, code);
  if (problem) throw new Error(problem);
  const base = { name: names.name.trim(), hubName: names.hubName.trim() };
  if (isCertificateAddress(address))
    return { ...base, url: address.trim(), code: code.trim() };
  return { ...base, address: address.trim(), code: normalizeCode(code)! };
}

/** A pairing or connection error as the forms say it: machine tokens off, and the
 * three cases the contract names explained. */
export function pairErrorText(error: unknown): string {
  const text = errorMessage(error);
  if (/PIN_MISMATCH/.test(text))
    return `${text.replace(/PIN_MISMATCH:\s*/g, "")} If the hub's key was reset (twr phone rekey), forget it here and pair again.`;
  return text.replace(/^(?:Error:\s*)?(?:BAD_CODE|UNREACHABLE):\s*/, "");
}
