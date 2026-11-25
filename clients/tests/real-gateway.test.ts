// The desktop's pinned transport against a real `twr gateway` (docs/design/phone-pairing.md).
// Skipped unless TWR_REAL_GATEWAY_PAIR names a command that opens an invitation on a running
// pinned gateway and prints its pair-json line (`twr gateway --pair --pair-only --json` in the
// gateway's environment), and TWR_REAL_GATEWAY_ADDRESS is its loopback `host:port`.
// clients/scripts/real-gateway-qa.sh runs it against a disposable hub and gateway.
import { describe, expect, it } from "vitest";
import { execFileSync } from "node:child_process";
import { DesktopTransport } from "../desktop/remote-transport";

const command = process.env.TWR_REAL_GATEWAY_PAIR;
const address = process.env.TWR_REAL_GATEWAY_ADDRESS;

function invite(): {
  code: string;
  gateway_id: string;
  invitation: {
    pin: string;
    id: string;
    routes: string[];
    iroh?: string;
    code: string;
  };
} {
  const out = execFileSync("sh", ["-c", command!], { encoding: "utf8" });
  return JSON.parse(out.trim().split("\n")[0]);
}

function transport() {
  const data = new Map<string, string>();
  const events: any[] = [];
  const native = new DesktopTransport(
    {
      ensureAvailable: () => {},
      read: async (key) => data.get(key) || null,
      write: async (key, value) => {
        data.set(key, value);
      },
      delete: async (key) => {
        data.delete(key);
      },
    },
    (event) => events.push(event),
    false,
  );
  return { native, events };
}

async function until<T>(get: () => T | undefined, ms = 15000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const value = get();
    if (value !== undefined) return value;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 50));
  }
}

describe.skipIf(!command || !address)("a real pinned gateway", () => {
  it("pairs from a scanned invitation, connects by pin and learns its routes", async () => {
    const { invitation } = invite();
    // The real gateway lists this machine's addresses; loopback is what reaches it here.
    const routes = [`tls:${address}`, ...invitation.routes];
    const { native, events } = transport();
    const hub = await native.pair({
      code: invitation.code,
      name: "real-gateway QA",
      hubName: "QA hub",
      pin: invitation.pin,
      gatewayId: invitation.id,
      routes,
      iroh: invitation.iroh,
    });
    expect(hub.pin).toBe(invitation.pin);
    expect(hub.route).toBe(`tls:${address}`);
    await native.connect({ id: hub.id });
    const hello = await until(() =>
      events.find((m) => m && m.type === "authenticated"),
    );
    expect(Array.isArray(hello.routes)).toBe(true);
    await native.disconnect();
  }, 60000);

  it("pairs by a typed address and code, and refuses a wrong code", async () => {
    const wrong = transport().native;
    invite();
    await expect(
      wrong.pair({
        code: "ABCD-EFGH-JK23",
        name: "wrong",
        hubName: "QA hub",
        address: address!,
      }),
    ).rejects.toThrow(/BAD_CODE|doesn't match/);
    const { code, invitation } = invite();
    const { native } = transport();
    const typed =
      `${code.slice(0, 4)}-${code.slice(4, 8)}-${code.slice(8)}`.toLowerCase();
    const hub = await native.pair({
      code: typed,
      name: "typed QA",
      hubName: "QA hub",
      address: address!,
    });
    expect(hub.pin).toBe(invitation.pin);
  }, 120000);
});
