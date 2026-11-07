import type { DesktopAPI } from "./types";
import type { Hub, NativeTransport } from "./remote";

export interface GatewayCheck {
  url: string;
  gatewayId: string;
  tls: boolean;
  externalReachable?: boolean;
  advisory?: string;
  restartRequired?: boolean;
  runningVersion?: string;
}
export interface GatewayStatus {
  running: boolean;
  runningVersion?: string;
  listen?: string;
  restartRequired?: boolean;
  renewalState?: string;
  renewalError?: string;
  logTail?: string;
}
export interface GatewaySettings {
  url: string;
  listen: string;
  cert: string;
  key: string;
  scopes: "read" | "operate" | "terminal" | "admin";
}
export interface Invitation {
  url: string;
  code: string;
  expiresAt: number;
  scopes?: string[];
}
export interface DesktopConnections extends NativeTransport {
  resetSavedHubs(): Promise<void>;
  check(request: { url: string }): Promise<GatewayCheck>;
  setup(request: GatewaySettings): Promise<GatewayCheck>;
  restart(request: GatewaySettings): Promise<GatewayCheck>;
  invite(request: { scopes: GatewaySettings["scopes"] }): Promise<Invitation>;
  settings(): Promise<GatewaySettings | null>;
  gatewayStatus(): Promise<GatewayStatus>;
  cancelInvitation(): Promise<void>;
  stopGateway(): Promise<void>;
  deactivateLocal(): Promise<void>;
}
/** QR invitations contain a short-lived, single-use code, never saved credentials. */
export function encodeInvitation(value: Invitation): string {
  return JSON.stringify({
    type: "twr-pair",
    version: 1,
    url: value.url,
    code: value.code,
    expiresAt: value.expiresAt,
  });
}
export function parseInvitation(raw: string, _now = Date.now()): Invitation {
  if (raw.length > 2048) throw new Error("This QR code is too large");
  let value: any;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new Error("Scan a twr pairing invitation");
  }
  if (
    !value ||
    value.type !== "twr-pair" ||
    value.version !== 1 ||
    typeof value.url !== "string" ||
    typeof value.code !== "string" ||
    !/^[a-zA-Z0-9-]{4,128}$/.test(value.code) ||
    !Number.isSafeInteger(value.expiresAt)
  )
    throw new Error("Scan a twr pairing invitation");
  const url = new URL(value.url);
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    !["", "/"].includes(url.pathname)
  )
    throw new Error("The invitation must use an HTTPS hub URL");
  // The hub enforces the five-minute lifetime. Device clocks may disagree.
  if (value.expiresAt <= 0) throw new Error("Invalid invitation expiry");
  if (
    Object.keys(value).some(
      (key) => !["type", "version", "url", "code", "expiresAt"].includes(key),
    )
  )
    throw new Error("Unexpected invitation data");
  return { url: url.origin, code: value.code, expiresAt: value.expiresAt };
}
export type SavedHub = Hub;
declare global {
  interface Window {
    twrLocal: DesktopAPI;
    twrRemote: DesktopConnections;
  }
}
