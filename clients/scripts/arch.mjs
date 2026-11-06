import {
  mkdirSync,
  cpSync,
  readFileSync,
  writeFileSync,
  rmSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { checkLinuxBundle } from "./linux-abi.mjs";
if (process.platform !== "linux" || process.arch !== "x64")
  throw new Error("Create the Arch package on x86_64 Linux.");
const { version } = JSON.parse(readFileSync("package.json", "utf8"));
const abi = checkLinuxBundle("release/linux-unpacked");
writeFileSync("release/linux-abi.json", JSON.stringify(abi, null, 2) + "\n");
const stage = "release/arch-stage/twr-desktop";
rmSync(stage, { recursive: true, force: true });
mkdirSync(stage, { recursive: true });
cpSync("release/linux-unpacked", stage, { recursive: true });
mkdirSync(stage + "/packaging", { recursive: true });
for (const name of ["LICENSE", "NOTICE"])
  cpSync("../" + name, stage + "/" + name);
cpSync("packaging/icon.svg", stage + "/packaging/icon.svg");
for (const name of ["twr-desktop", "tools.twr.desktop.desktop"])
  cpSync("packaging/arch/" + name, stage + "/packaging/" + name);
const out = "release/arch";
mkdirSync(out, { recursive: true });
const filename = `twr-desktop-${version}-linux-x64.tar.gz`;
const tar = spawnSync(
  "tar",
  ["-czf", `${out}/${filename}`, "-C", "release/arch-stage", "twr-desktop"],
  { stdio: "inherit" },
);
if (tar.status !== 0) process.exit(tar.status || 1);
const hash = createHash("sha256")
  .update(readFileSync(`${out}/${filename}`))
  .digest("hex");
const template = readFileSync("packaging/arch/PKGBUILD.in", "utf8");
writeFileSync(
  `${out}/PKGBUILD`,
  template.replaceAll("@VERSION@", version).replace("@SHA256@", hash),
);
console.log(
  `Arch sources prepared in ${out}. Run makepkg there to build the installable package.`,
);
