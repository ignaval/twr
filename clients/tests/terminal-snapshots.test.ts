import { describe, expect, it } from "vitest";
import { TerminalSnapshots } from "../shared/terminalSnapshots";
describe("terminal bootstrap snapshots", () => {
  it("cannot resurrect a terminal closed while reconnect bootstrap was in flight", () => {
    const snapshots = new TerminalSnapshots();
    const revision = snapshots.begin();
    snapshots.changed(); // close submitted
    snapshots.removed("closed"); // server acknowledged
    expect(snapshots.apply(revision, [{ id: "closed" }])).toBeUndefined();
    // A queued resync may itself have captured an older server result.
    expect(
      snapshots.apply(snapshots.begin(), [{ id: "closed" }, { id: "live" }]),
    ).toEqual([{ id: "live" }]);
  });
  it("does not replace a newly opened terminal with a snapshot captured before it existed", () => {
    const snapshots = new TerminalSnapshots();
    const revision = snapshots.begin();
    snapshots.changed();
    expect(snapshots.apply(revision, [])).toBeUndefined();
    expect(snapshots.apply(snapshots.begin(), [{ id: "new" }])).toEqual([
      { id: "new" },
    ]);
  });
});
