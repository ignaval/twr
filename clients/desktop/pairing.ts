import { parseInvitation } from "../shared/connections";
import { errorMessage } from "../shared/errors";
import { displayRoute, looksLikeInvitation } from "../shared/phone-pairing";
import {
  PAIR_LIMITS,
  addressProblem,
  hubNameFor,
  invitationAddresses,
  invitationCode,
  invitationHubName,
  isCertificateAddress,
  requestFromAddress,
  requestFromInvitation,
} from "../shared/pair-form";
import type { PairRequest } from "../shared/remote";

export { PAIR_LIMITS, hubNameFor };

/** The Pair form's fields (Connections.tsx › Connect to another computer). `url` is the
 * hub address: a host or IP address (typed, pinned pairing) or an `https://` URL
 * (certificate mode). */
export interface PairFields {
  hubName: string;
  url: string;
  code: string;
  name: string;
}

/** The fields a pasted invitation fills: its address and code, and a hub name when the
 * form has none yet. Undefined for text that isn't an invitation at all (typing in the
 * box); an error for one that is but is wrong. A version 2 invitation pairs by its key
 * and all its routes; the address shown is its first. */
export function fieldsFromInvitation(
  text: string,
  current: PairFields,
): PairFields | undefined {
  const raw = text.trim();
  if (!looksLikeInvitation(raw)) return undefined;
  const invitation = parseInvitation(raw);
  const address = invitationAddresses(invitation)[0];
  return {
    ...current,
    url:
      invitation.version === 2
        ? address
          ? displayRoute(address)
          : ""
        : invitation.url,
    code: invitationCode(invitation),
    hubName: current.hubName.trim() || invitationHubName(invitation),
  };
}

/** What the Pair form still needs, in words, or nothing when it can pair. */
export function missingPairFields(fields: PairFields): string | undefined {
  const missing = [
    !fields.url.trim() && "the hub's address",
    !fields.code.trim() && "its one-use code",
    !fields.hubName.trim() && "a hub name",
    !fields.name.trim() && "this device's name",
  ].filter(Boolean) as string[];
  if (!missing.length) return undefined;
  return `Paste an invitation, or enter ${
    missing.length === 1
      ? missing[0]
      : missing.slice(0, -1).join(", ") + " and " + missing.at(-1)
  }.`;
}

/** What is wrong with details that are all there, in words (the limits `twrRemote.pair`
 * checks), or nothing when they can be sent. */
export function pairFieldProblem(fields: {
  url: string;
  code: string;
  hubName: string;
  name: string;
}): string | undefined {
  const names = nameProblem(fields);
  if (names) return names;
  const problem = addressProblem(fields.url, fields.code);
  if (!problem) return undefined;
  return isCertificateAddress(fields.url) && /one-use code/.test(problem)
    ? problem + " Copy it again from the invitation on the other computer."
    : problem;
}

function nameProblem(fields: { hubName: string; name: string }) {
  if (fields.hubName.trim().length > PAIR_LIMITS.name)
    return `The hub name is too long. Use at most ${PAIR_LIMITS.name} characters.`;
  if (fields.name.trim().length > PAIR_LIMITS.name)
    return `This device's name is too long. Use at most ${PAIR_LIMITS.name} characters.`;
  return undefined;
}

/** The fields an invitation filled in (`filled`), emptied again: its text was cleared or
 * changed into something that isn't one. A field changed by hand since is kept. */
export function withoutInvitation(
  fields: PairFields,
  filled: Partial<PairFields> | undefined,
): PairFields {
  if (!filled) return fields;
  const next = { ...fields };
  for (const key of ["url", "code", "hubName"] as const)
    if (filled[key] !== undefined && next[key] === filled[key]) next[key] = "";
  return next;
}

/** What **Pair and connect** sends. With text in Paste invitation, what that text says now:
 * text that isn't a valid invitation is refused, whatever an earlier one filled in; a
 * version 2 one pairs by its key. Without, the details entered by hand: an `https://`
 * address in certificate mode, a host or IP address by typed pairing. `manual`: the error
 * names a detail to enter by hand. */
export function pairRequest(
  invitationText: string,
  fields: PairFields,
):
  | { fields: PairFields; request: PairRequest }
  | { error: string; manual: boolean } {
  const pasted = !!invitationText.trim();
  if (pasted) {
    const again =
      " Copy it again on the other computer, or clear Paste invitation to enter the details manually.";
    try {
      const filled = fieldsFromInvitation(invitationText, fields);
      if (!filled)
        return {
          error: "The pasted text isn't a twr invitation." + again,
          manual: false,
        };
      const missing = missingPairFields(filled) ?? nameProblem(filled);
      if (missing) return { error: missing, manual: true };
      return {
        fields: filled,
        request: requestFromInvitation(
          parseInvitation(invitationText.trim()),
          filled,
        ),
      };
    } catch (error) {
      return {
        error: `The pasted invitation can't be used: ${errorMessage(error).replace(/\.$/, "")}.${again}`,
        manual: false,
      };
    }
  }
  const missing = missingPairFields(fields) ?? pairFieldProblem(fields);
  if (missing) return { error: missing, manual: false };
  return {
    fields,
    request: requestFromAddress(fields.url, fields.code, fields),
  };
}
