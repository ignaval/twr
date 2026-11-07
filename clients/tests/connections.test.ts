import { describe, expect, it } from "vitest";
import { encodeInvitation, parseInvitation } from "../shared/connections";
import { gatewayUrl } from "../desktop/remote-transport";
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
