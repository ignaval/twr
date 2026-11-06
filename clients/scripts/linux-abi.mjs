// Release packages target Ubuntu 22.04's ABI, including native modules rebuilt
// by electron-builder. Checking the packaged files catches a newer build host.
import {
  closeSync,
  openSync,
  readSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const ABI_LIMITS = {
  GLIBC: "2.35",
  GLIBCXX: "3.4.30",
  CXXABI: "1.3.13",
};

function checkedSlice(data, start, size) {
  if (
    !Number.isSafeInteger(start) ||
    !Number.isSafeInteger(size) ||
    start < 0 ||
    size < 0 ||
    start + size > data.length
  )
    throw new Error("Truncated or invalid ELF section");
  return data.subarray(start, start + size);
}

function stringAt(table, offset) {
  const end = table.indexOf(0, offset);
  if (offset >= table.length || end < offset)
    throw new Error("Invalid ELF version string");
  return table.toString("utf8", offset, end);
}

// Read only GNU version *requirements*, not arbitrary binary strings or symbol
// definitions. This also runs on macOS for inspecting downloaded Linux bundles.
export function elfRequirements(data) {
  if (
    data.length < 64 ||
    data.toString("hex", 0, 4) !== "7f454c46" ||
    data[4] !== 2 ||
    data[5] !== 1 ||
    data.readUInt16LE(18) !== 62
  )
    throw new Error("Expected a little-endian x86_64 ELF binary");
  const sectionOffset = Number(data.readBigUInt64LE(40));
  const sectionSize = data.readUInt16LE(58),
    count = data.readUInt16LE(60);
  if (sectionSize < 64 || count === 0)
    throw new Error("Missing ELF section table");
  const section = (index) => {
    if (index >= count) throw new Error("Invalid ELF section link");
    const header = checkedSlice(
      data,
      sectionOffset + index * sectionSize,
      sectionSize,
    );
    return {
      type: header.readUInt32LE(4),
      offset: Number(header.readBigUInt64LE(24)),
      size: Number(header.readBigUInt64LE(32)),
      link: header.readUInt32LE(40),
    };
  };
  const requirements = new Set();
  for (let index = 0; index < count; index++) {
    const item = section(index);
    if (item.type !== 0x6ffffffe) continue; // SHT_GNU_verneed
    const strings = section(item.link);
    const table = checkedSlice(data, strings.offset, strings.size);
    const versions = checkedSlice(data, item.offset, item.size);
    let position = 0;
    while (position < versions.length) {
      const header = checkedSlice(versions, position, 16);
      if (header.readUInt16LE(0) !== 1)
        throw new Error("Unsupported GNU version requirement");
      const auxiliaryCount = header.readUInt16LE(2);
      let auxiliary = position + header.readUInt32LE(8);
      for (let n = 0; n < auxiliaryCount; n++) {
        const entry = checkedSlice(versions, auxiliary, 16);
        requirements.add(stringAt(table, entry.readUInt32LE(8)));
        const next = entry.readUInt32LE(12);
        if (n + 1 < auxiliaryCount && next < 16)
          throw new Error("Invalid GNU version chain");
        auxiliary += next;
      }
      const next = header.readUInt32LE(12);
      if (!next) break;
      if (next < 16) throw new Error("Invalid GNU version chain");
      position += next;
    }
  }
  return [...requirements].sort();
}

function newerThan(version, limit) {
  const current = version.split(".").map(Number),
    baseline = limit.split(".").map(Number);
  for (
    let index = 0;
    index < Math.max(current.length, baseline.length);
    index++
  ) {
    const delta = (current[index] || 0) - (baseline[index] || 0);
    if (delta) return delta > 0;
  }
  return false;
}

export function incompatibleRequirements(requirements) {
  return requirements.filter((name) => {
    const family = Object.keys(ABI_LIMITS).find((prefix) =>
      name.startsWith(prefix + "_"),
    );
    if (!family) return false;
    const version = name.slice(family.length + 1);
    return (
      !/^\d+(\.\d+)*$/.test(version) || newerThan(version, ABI_LIMITS[family])
    );
  });
}

function isElf(path) {
  const descriptor = openSync(path, "r");
  try {
    const magic = Buffer.alloc(4);
    return (
      readSync(descriptor, magic, 0, 4, 0) === 4 &&
      magic.toString("hex") === "7f454c46"
    );
  } finally {
    closeSync(descriptor);
  }
}

export function checkLinuxBundle(directory) {
  const root = resolve(directory);
  const required = [
    "twr-desktop",
    "resources/bin/twr",
    "resources/bin/twr-desktop-bridge",
    "resources/bin/twr-mobile-gateway",
    "resources/app.asar.unpacked/node_modules/node-pty/build/Release/pty.node",
  ];
  for (const name of required)
    if (!isElf(join(root, name)))
      throw new Error(`Missing packaged ELF binary: ${name}`);
  const binaries = [];
  const walk = (directory) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) walk(path);
      else if (entry.isFile() && isElf(path)) {
        const requirements = elfRequirements(readFileSync(path));
        const incompatible = incompatibleRequirements(requirements);
        binaries.push({
          path: relative(root, path),
          requirements,
          incompatible,
        });
      }
    }
  };
  walk(root);
  const report = {
    baseline: "Ubuntu 22.04 x86_64",
    limits: ABI_LIMITS,
    binaries,
  };
  const failures = binaries.filter((item) => item.incompatible.length);
  if (failures.length)
    throw new Error(
      "Linux release ABI exceeds Ubuntu 22.04. Build the portable bundle on Ubuntu 22.04 (see Desktop CI); native development on newer Linux remains supported.\n" +
        failures
          .map((item) => `${item.path}: ${item.incompatible.join(", ")}`)
          .join("\n"),
    );
  return report;
}

export default function afterPack(context) {
  if (context.electronPlatformName !== "linux") return;
  const report = checkLinuxBundle(context.appOutDir);
  const output = join(context.outDir, "linux-abi.json");
  writeFileSync(output, JSON.stringify(report, null, 2) + "\n");
  console.log(
    `Verified ${report.binaries.length} Linux ELF files against Ubuntu 22.04; report: ${output}`,
  );
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const report = checkLinuxBundle(process.argv[2] || "release/linux-unpacked");
  if (process.argv[3])
    writeFileSync(process.argv[3], JSON.stringify(report, null, 2) + "\n");
  console.log(JSON.stringify(report, null, 2));
}
