import { expect, it } from "vitest";
import {
  elfRequirements,
  incompatibleRequirements,
} from "../scripts/linux-abi.mjs";

function elfWithRequirements(names: string[]) {
  const strings = Buffer.from("\0" + names.join("\0") + "\0");
  const start = 64 + 3 * 64,
    versionsStart = start + strings.length;
  const data = Buffer.alloc(versionsStart + 16 + 16 * names.length);
  data.write("\x7fELF");
  data[4] = 2;
  data[5] = 1;
  data.writeUInt16LE(62, 18);
  data.writeBigUInt64LE(64n, 40);
  data.writeUInt16LE(64, 58);
  data.writeUInt16LE(3, 60);
  data.writeUInt32LE(3, 64 + 64 + 4); // string table
  data.writeBigUInt64LE(BigInt(start), 64 + 64 + 24);
  data.writeBigUInt64LE(BigInt(strings.length), 64 + 64 + 32);
  data.writeUInt32LE(0x6ffffffe, 64 + 128 + 4);
  data.writeBigUInt64LE(BigInt(versionsStart), 64 + 128 + 24);
  data.writeBigUInt64LE(BigInt(16 + 16 * names.length), 64 + 128 + 32);
  data.writeUInt32LE(1, 64 + 128 + 40);
  strings.copy(data, start);
  data.writeUInt16LE(1, versionsStart);
  data.writeUInt16LE(names.length, versionsStart + 2);
  data.writeUInt32LE(16, versionsStart + 8);
  let offset = 1;
  names.forEach((name, index) => {
    const entry = versionsStart + 16 + 16 * index;
    data.writeUInt32LE(offset, entry + 8);
    data.writeUInt32LE(index + 1 < names.length ? 16 : 0, entry + 12);
    offset += Buffer.byteLength(name) + 1;
  });
  return data;
}

it("reads actual ELF version requirements and rejects newer libc and C++ runtimes", () => {
  const versions = [
    "GLIBC_2.35",
    "GLIBC_2.42",
    "GLIBCXX_3.4.30",
    "GLIBCXX_3.4.31",
    "CXXABI_1.3.13",
    "CXXABI_1.3.14",
  ];
  expect(elfRequirements(elfWithRequirements(versions))).toEqual(
    [...versions].sort(),
  );
  expect(incompatibleRequirements(versions)).toEqual([
    "GLIBC_2.42",
    "GLIBCXX_3.4.31",
    "CXXABI_1.3.14",
  ]);
  expect(
    incompatibleRequirements([
      "GLIBC_2.9",
      "GLIBC_2.34",
      "GLIBCXX_3.4.9",
      "GCC_3.0",
    ]),
  ).toEqual([]);
  expect(
    incompatibleRequirements(["GLIBC_ABI_DT_RELR", "GLIBC_PRIVATE"]),
  ).toEqual(["GLIBC_ABI_DT_RELR", "GLIBC_PRIVATE"]);
});

it("fails closed on an invalid or truncated ELF version table", () => {
  expect(() => elfRequirements(Buffer.from("not ELF"))).toThrow();
  const data = elfWithRequirements(["GLIBC_2.42"]);
  expect(() => elfRequirements(data.subarray(0, data.length - 2))).toThrow(
    "Truncated",
  );
  data.writeUInt32LE(99, 64 + 128 + 40);
  expect(() => elfRequirements(data)).toThrow("section link");
});
