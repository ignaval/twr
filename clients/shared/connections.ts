import type { DesktopAPI } from "./types";
import type { Hub, NativeTransport } from "./remote";
import {
  encodeInvitationJson,
  invitationLink,
  isPinnedInvitation,
  parseAnyInvitation,
  type Invitation,
  type InvitationV1,
  type InvitationV2,
} from "./phone-pairing";

export interface GatewayCheck {
  url: string;
  gatewayId: string;
  tls: boolean;
  externalReachable?: boolean;
  advisory?: string;
  restartRequired?: boolean;
  runningVersion?: string;
  /** Pinned mode: the gateway's key, and the addresses an invitation would carry now. */
  pin?: string;
  routes?: string[];
}
export interface GatewayStatus {
  running: boolean;
  runningVersion?: string;
  listen?: string;
  restartRequired?: boolean;
  renewalState?: string;
  renewalError?: string;
  logTail?: string;
  /** Set when the running gateway serves its own key (pinned mode). */
  pin?: string;
  routes?: string[];
}
/** `pinned` (the default): the gateway serves its own key and devices pin it. `certificate`:
 * the gateway serves `cert`/`key` for the public HTTPS `url`, trusted by the system. */
export type GatewayMode = "pinned" | "certificate";
export interface GatewaySettings {
  /** Absent in settings saved before pinned mode existed: those are certificate mode. */
  mode?: GatewayMode;
  url: string;
  listen: string;
  cert: string;
  key: string;
  scopes: "read" | "operate" | "terminal" | "admin";
}
export function gatewayMode(
  settings: Pick<GatewaySettings, "mode">,
): GatewayMode {
  return settings.mode ?? "certificate";
}
export type { Invitation, InvitationV1, InvitationV2 };
/** A created invitation; a pinned one carries the gateway's `twr://pair#` link too. */
export type CreatedInvitation =
  InvitationV1 | (InvitationV2 & { link: string });
export interface DesktopConnections extends NativeTransport {
  resetSavedHubs(): Promise<void>;
  check(request: { url: string }): Promise<GatewayCheck>;
  setup(request: GatewaySettings): Promise<GatewayCheck>;
  restart(request: GatewaySettings): Promise<GatewayCheck>;
  invite(request: {
    scopes: GatewaySettings["scopes"];
  }): Promise<CreatedInvitation>;
  settings(): Promise<GatewaySettings | null>;
  gatewayStatus(): Promise<GatewayStatus>;
  cancelInvitation(): Promise<void>;
  stopGateway(): Promise<void>;
  deactivateLocal(): Promise<void>;
}
/** The QR code's text: the invitation JSON, version 1 or 2. QR invitations contain a
 * short-lived, single-use code, never saved credentials. */
export function encodeInvitation(value: Invitation): string {
  return encodeInvitationJson(value);
}
/** What Copy invitation copies: a pinned hub's `twr://pair#` link, or a certificate-mode
 * hub's version 1 JSON (which has no link form). */
export function invitationText(value: Invitation): string {
  return isPinnedInvitation(value)
    ? invitationLink(value)
    : encodeInvitationJson(value);
}
/** Version 1 JSON, version 2 JSON or a `twr://pair#` link (docs/design/phone-pairing.md).
 * The hub enforces expiry; device clocks may disagree, so `_now` is unused. */
export function parseInvitation(raw: string, _now = Date.now()): Invitation {
  return parseAnyInvitation(raw);
}
export type SavedHub = Hub;
declare global {
  interface Window {
    twrLocal: DesktopAPI;
    twrRemote: DesktopConnections;
  }
}
