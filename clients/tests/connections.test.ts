import { describe, expect, it } from "vitest";
import { encodeInvitation, parseInvitation } from "../shared/connections";
import { gatewayUrl } from "../desktop/remote-transport";
import {
  fieldsFromInvitation,
  hubNameFor,
  missingPairFields,
  pairFieldProblem,
  pairRequest,
  withoutInvitation,
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
  it("pairs with what the pasted text says now, never an earlier invitation", () => {
    const text = encodeInvitation(invitation);
    const filled = fieldsFromInvitation(text, empty)!;
    expect(pairRequest(text, filled)).toMatchObject({
      fields: filled,
      request: { url: invitation.url, code: invitation.code },
    });
    // Replaced with something else: refused, though the fields still hold the old one.
    const other = pairRequest("not an invitation", filled);
    expect(other).toMatchObject({ manual: false });
    expect("error" in other && other.error).toMatch(
      /^The pasted text isn't a twr invitation\. Copy it again/,
    );
    const broken = pairRequest('{"type":"twr-pair"', filled);
    expect("error" in broken && broken.error).toMatch(
      /^The pasted invitation can't be used: Scan a twr pairing invitation\. /,
    );
    // The invitation in the box wins over an address typed under the manual details.
    expect(
      pairRequest(text, { ...filled, url: "https://elsewhere.example" }),
    ).toMatchObject({ fields: filled, request: { url: invitation.url } });
    // Nothing pasted: the details entered by hand.
    const manual = {
      hubName: "Lab",
      url: "https://lab.example:47321",
      code: "zzzz-1111",
      name: "laptop",
    };
    expect(pairRequest("  ", manual)).toEqual({
      fields: manual,
      request: {
        url: manual.url,
        code: manual.code,
        name: manual.name,
        hubName: manual.hubName,
      },
    });
    expect(pairRequest("", empty)).toEqual({
      error: missingPairFields(empty),
      manual: false,
    });
    // A valid invitation, but the device's name was cleared: the manual details open.
    expect(pairRequest(text, { ...filled, name: "" })).toMatchObject({
      manual: true,
    });
  });
  it("clears what an invitation filled once its text goes, keeping what was typed", () => {
    const filled = fieldsFromInvitation(encodeInvitation(invitation), empty)!;
    const loaded = { url: filled.url, code: filled.code, hubName: "hub" };
    expect(withoutInvitation(filled, loaded)).toEqual({
      ...empty,
      name: "twr desktop",
    });
    expect(
      withoutInvitation({ ...filled, hubName: "Studio" }, loaded).hubName,
    ).toBe("Studio");
    expect(withoutInvitation(filled, undefined)).toBe(filled);
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

describe("the connection notice", () => {
  it("says the hub reconnecting in words, the socket path only as its detail", async () => {
    const { connectionWords } = await import("../shared/errors");
    const raw = "connecting to hub at /private/var/folders/x/hub.sock";
    expect(connectionWords(raw)).toEqual({
      text: "Connecting to the hub…",
      detail: raw,
    });
    expect(connectionWords("Hub connection closed; reconnecting").text).toBe(
      "Connecting to the hub…",
    );
    expect(connectionWords("Configuration needs repair")).toEqual({
      text: "Configuration needs repair",
    });
  });
});

describe("details entered by hand", () => {
  const manual = {
    hubName: "Lab",
    url: "https://lab.example:47321",
    code: "zzzz-1111",
    name: "laptop",
  };
  it("says what is wrong in a sentence, never zod's issue list", () => {
    expect(pairFieldProblem(manual)).toBeUndefined();
    expect(pairFieldProblem({ ...manual, code: "abc" })).toMatch(
      /^The one-use code is too short\./,
    );
    expect(pairFieldProblem({ ...manual, code: "x".repeat(129) })).toMatch(
      /^The one-use code is too long\./,
    );
    expect(pairFieldProblem({ ...manual, name: "n".repeat(81) })).toBe(
      "This device's name is too long. Use at most 80 characters.",
    );
    expect(pairFieldProblem({ ...manual, url: "https://" })).toMatch(
      /^The hub address isn't a web address\./,
    );
    // A host or IP address is typed pairing: it needs a twelve-character code.
    expect(pairFieldProblem({ ...manual, url: "lab:47321" })).toMatch(
      /^The pairing code has 12 letters and digits/,
    );
    expect(
      pairFieldProblem({ ...manual, url: "lab:47321", code: "abcd efgh jk23" }),
    ).toBeUndefined();
    expect(pairFieldProblem({ ...manual, url: "lab:99999" })).toMatch(
      /^The hub address can't be used/,
    );
    expect(
      pairRequest("", {
        ...manual,
        url: "192.168.1.20",
        code: "abcd-efgh-jk23",
      }),
    ).toEqual({
      fields: { ...manual, url: "192.168.1.20", code: "abcd-efgh-jk23" },
      request: {
        address: "192.168.1.20",
        code: "ABCDEFGHJK23",
        name: "laptop",
        hubName: "Lab",
      },
    });
    // Pair and connect refuses it before asking the main process.
    expect(pairRequest("", { ...manual, code: "abc" })).toEqual({
      error: pairFieldProblem({ ...manual, code: "abc" }),
      manual: false,
    });
  });
});
