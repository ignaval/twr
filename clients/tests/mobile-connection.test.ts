import { describe, expect, it } from "vitest";
import { linkState } from "../mobile/src/connection";

describe("mobile header status", () => {
  it("is online while connected, reconnecting while retrying, offline once revoked", () => {
    expect(linkState(true, false)).toBe("online");
    expect(linkState(false, false, "Remote hub connection closed")).toBe(
      "reconnecting",
    );
    expect(
      linkState(false, false, "Gateway refused the connection (403)"),
    ).toBe("reconnecting");
    expect(linkState(false, true)).toBe("offline");
    expect(
      linkState(
        false,
        false,
        "AUTH_REVOKED: This device no longer has access to the hub.",
      ),
    ).toBe("offline");
  });
});
