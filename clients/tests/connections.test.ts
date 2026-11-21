import { describe, expect, it } from "vitest";
import { encodeInvitation, parseInvitation } from "../shared/connections";
import { gatewayUrl } from "../desktop/remote-transport";
import {
  fieldsFromInvitation,
  hubNameFor,
  missingPairFields,
} from "../desktop/pairing";
const now = 1_800_000_000_000;
const invitation = {
  url: "https://hub.example:47321",
  code: "abcdef-123456",
  expiresAt: now + 300_000,
};
describe("one-use QR invitation format", () => {
  it("encodes only an HTTPS endpoint and expiring one-use code", () => {
    const encoded = encodeInvitation(invitation);
    expect(parseInvitation(encoded, now)).toEqual(invitation);
    expect(Object.keys(JSON.parse(encoded)).sort()).toEqual([
      "code",
      "expiresAt",
      "type",
      "url",
      "version",
    ]);
  });
  it("leaves clock-dependent expiry enforcement to the hub", () => {
    for (const expiresAt of [now - 600_000, now + 600_000])
      expect(
        parseInvitation(encodeInvitation({ ...invitation, expiresAt }), now)
          .expiresAt,
      ).toBe(expiresAt);
  });
  it("rejects malformed, credential-bearing and redirected invitations", () => {
    for (const update of [
      { expiresAt: 0 },
      { expiresAt: 1.5 },
      { url: "http://localhost:47321" },
      { url: "https://user:secret@hub.example" },
      { url: "https://hub.example/v1/pair" },
      { url: "https://hub.example?token=abc" },
      { token: "persisted-secret" },
      { code: "\nwrong" },
    ])
      expect(() =>
        parseInvitation(
          JSON.stringify({
            ...JSON.parse(encodeInvitation(invitation)),
            ...update,
          }),
          now,
        ),
      ).toThrow();
    expect(() => parseInvitation("x".repeat(2049), now)).toThrow("large");
  });
});
describe("native gateway URL boundary", () => {
  it("requires HTTPS and a bare origin in production", () => {
    expect(gatewayUrl("https://hub.example:47321/")).toBe(
      "https://hub.example:47321",
    );
    for (const value of [
      "http://localhost:47321",
      "https://u:p@hub.example",
      "https://hub.example/path",
      "https://hub.example?q=1",
      "https://hub.example/#secret",
      "file:///tmp/hub",
    ])
      expect(() => gatewayUrl(value)).toThrow();
  });
  it("allows only loopback plaintext in development", () => {
    expect(gatewayUrl("http://127.0.0.1:1234", true)).toBe(
      "http://127.0.0.1:1234",
    );
    expect(gatewayUrl("http://[::1]:1234", true)).toBe("http://[::1]:1234");
    expect(() => gatewayUrl("http://192.168.1.4:1234", true)).toThrow();
  });
});

describe("pasting an invitation into Connect to another computer", () => {
  const empty = { hubName: "", url: "", code: "", name: "twr desktop" };
  it("fills the address and code, and names the hub after its host", () => {
    expect(fieldsFromInvitation(encodeInvitation(invitation), empty)).toEqual({
      hubName: "hub",
      url: "https://hub.example:47321",
      code: "abcdef-123456",
      name: "twr desktop",
    });
    expect(
      fieldsFromInvitation(" " + encodeInvitation(invitation) + "\n", {
        ...empty,
        hubName: "Studio",
      })?.hubName,
    ).toBe("Studio");
  });
  it("leaves typing alone and refuses a broken invitation", () => {
    expect(fieldsFromInvitation("https://hub", empty)).toBeUndefined();
    expect(() => fieldsFromInvitation('{"type":"other"}', empty)).toThrow(
      "Scan a twr pairing invitation",
    );
  });
  it("names an IP address hub by its address", () => {
    expect(hubNameFor("https://192.168.1.4:47321")).toBe("192.168.1.4");
    expect(hubNameFor("https://[::1]:47321")).toBe("::1");
    expect(hubNameFor("https://workstation:47321")).toBe("workstation");
  });
  it("says what the form still needs", () => {
    expect(missingPairFields(empty)).toBe(
      "Paste an invitation, or enter the hub's address, its one-use code and a hub name.",
    );
    expect(
      missingPairFields({
        hubName: "a",
        url: "https://a",
        code: "c",
        name: "d",
      }),
    ).toBeUndefined();
  });
});
