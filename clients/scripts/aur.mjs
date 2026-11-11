// Writes the AUR package (twr-desktop-bin) for a published release:
//   node scripts/aur.mjs <portable archive> [output directory]
// The archive is the twr-desktop-<version>-linux-x64.tar.gz release asset that
// scripts/arch.mjs produces. The AUR package downloads that same asset.
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";

export function aurFiles(template, version, sha256) {
  if (!/^\d+\.\d+\.\d+$/.test(version))
    throw new Error(
      `The AUR package tracks stable releases only, not ${version}`,
    );
  if (!/^[a-f0-9]{64}$/.test(sha256)) throw new Error("Invalid SHA-256");
  const pkgbuild = template
    .replaceAll("@VERSION@", version)
    .replace("@SHA256@", sha256);
  return { pkgbuild, srcinfo: srcinfo(pkgbuild) };
}

/** The .SRCINFO makepkg --printsrcinfo emits for this PKGBUILD's fields. */
export function srcinfo(pkgbuild) {
  const scalar = (name) => {
    const match = new RegExp(
      `^${name}=(?:'([^']*)'|"([^"]*)"|(\\S+))$`,
      "m",
    ).exec(pkgbuild);
    if (!match) throw new Error(`PKGBUILD has no ${name}`);
    return match[1] ?? match[2] ?? match[3];
  };
  const array = (name) => {
    const match = new RegExp(`^${name}=\\(([^)]*)\\)$`, "m").exec(pkgbuild);
    if (!match) return [];
    return [...match[1].matchAll(/'([^']*)'|"([^"]*)"/g)].map(
      (m) => m[1] ?? m[2],
    );
  };
  const pkgname = scalar("pkgname");
  const pkgver = scalar("pkgver");
  const expand = (value) =>
    value.replaceAll("${pkgname}", pkgname).replaceAll("${pkgver}", pkgver);
  const lines = [`pkgbase = ${pkgname}`];
  const add = (key, values) => {
    for (const value of values) lines.push(`\t${key} = ${expand(value)}`);
  };
  add("pkgdesc", [scalar("pkgdesc")]);
  add("pkgver", [pkgver]);
  add("pkgrel", [scalar("pkgrel")]);
  add("url", [scalar("url")]);
  add("arch", array("arch"));
  add("license", array("license"));
  add("depends", array("depends"));
  add("optdepends", array("optdepends"));
  add("provides", array("provides"));
  add("conflicts", array("conflicts"));
  add("options", array("options"));
  add("source", array("source"));
  add("sha256sums", array("sha256sums"));
  lines.push("", `pkgname = ${pkgname}`, "");
  return lines.join("\n");
}

if (process.argv[1] && basename(process.argv[1]) === "aur.mjs") {
  const [archive, out = "release/aur"] = process.argv.slice(2);
  if (!archive)
    throw new Error("Usage: node scripts/aur.mjs <archive> [output]");
  const match = /twr-desktop-(\d+\.\d+\.\d+[^/]*)-linux-x64\.tar\.gz$/.exec(
    archive,
  );
  if (!match)
    throw new Error(`${archive} is not a twr-desktop portable archive`);
  const version = match[1];
  const { version: packageVersion } = JSON.parse(
    readFileSync("package.json", "utf8"),
  );
  if (version !== packageVersion)
    throw new Error(
      `Archive ${version} does not match package.json ${packageVersion}`,
    );
  const sha256 = createHash("sha256")
    .update(readFileSync(archive))
    .digest("hex");
  const files = aurFiles(
    readFileSync("packaging/aur/PKGBUILD.in", "utf8"),
    version,
    sha256,
  );
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, "PKGBUILD"), files.pkgbuild);
  writeFileSync(join(out, ".SRCINFO"), files.srcinfo);
  console.log(`AUR package twr-desktop-bin ${version} written to ${out}`);
}
