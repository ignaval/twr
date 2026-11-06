import { expect, it } from "vitest";
import { FrameAssembler } from "../mobile/src/framing";

function fragments(transfer: string, value: unknown) {
  const characters = Array.from(JSON.stringify(value));
  const pieces: string[] = [];
  for (let i = 0; i < characters.length; i += 4000)
    pieces.push(characters.slice(i, i + 4000).join(""));
  return pieces.map((data, index) => ({
    type: "frame-part",
    transfer,
    index,
    count: pieces.length,
    data,
  }));
}
it("reassembles interleaved Unicode replies while passing keepalives immediately", () => {
  const assembler = new FrameAssembler();
  const a = { id: "a", result: '界💬\\"'.repeat(20000) };
  const b = { id: "b", result: "second".repeat(10000) };
  const left = fragments("left", a),
    right = fragments("right", b);
  const completed: unknown[] = [];
  for (let i = 0; i < Math.max(left.length, right.length); i++) {
    for (const fragment of [left[i], right[i]]) {
      if (!fragment) continue;
      const result = assembler.accept(fragment);
      if (result) completed.push(result);
    }
    const heartbeat = { type: "heartbeat" };
    expect(assembler.accept(heartbeat)).toBe(heartbeat);
  }
  expect(completed).toContainEqual(a);
  expect(completed).toContainEqual(b);
});
it("cancels a stale workspace message before it becomes visible", () => {
  const assembler = new FrameAssembler();
  const old = fragments("old", {
    type: "state",
    workspace: "old",
    text: "x".repeat(10000),
  });
  expect(assembler.accept(old[0])).toBeUndefined();
  expect(
    assembler.accept({ type: "frame-cancel", transfer: "old" }),
  ).toBeUndefined();
  const current = { type: "state", workspace: "current" };
  expect(assembler.accept(fragments("new", current)[0])).toEqual(current);
});
it("rejects oversized, out-of-order and excessive concurrent transfers", () => {
  const assembler = new FrameAssembler();
  expect(() =>
    assembler.accept({
      type: "frame-part",
      transfer: "a",
      index: 1,
      count: 2,
      data: "x",
    }),
  ).toThrow("out of order");
  expect(() =>
    assembler.accept({
      type: "frame-part",
      transfer: "a",
      index: 0,
      count: 2,
      data: "界".repeat(16384),
    }),
  ).toThrow("limit");
  for (let i = 0; i < 4; i++)
    assembler.accept({
      type: "frame-part",
      transfer: String(i),
      index: 0,
      count: 2,
      data: "x",
    });
  expect(() =>
    assembler.accept({
      type: "frame-part",
      transfer: "fifth",
      index: 0,
      count: 2,
      data: "x",
    }),
  ).toThrow("incomplete");
  assembler.clear();
  const frame = {
    type: "frame-part",
    transfer: "large",
    count: 1024,
    data: "x".repeat(32768),
  };
  for (let index = 0; index < 512; index++)
    assembler.accept({ ...frame, index });
  expect(() => assembler.accept({ ...frame, index: 512 })).toThrow("16 MiB");
});
