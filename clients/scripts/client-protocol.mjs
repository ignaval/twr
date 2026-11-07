// Raw-socket QA clients must use the protocol of the hub built from this checkout.
import { readFileSync } from "node:fs";
const source = readFileSync(
  new URL("../../crates/twr-core/src/proto.rs", import.meta.url),
  "utf8",
);
const version = source.match(/^pub const PROTOCOL_VERSION: u32 = (\d+);$/m);
if (!version) throw new Error("Cannot read the hub's client protocol");
export const clientProtocol = Number(version[1]);
