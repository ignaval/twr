import { generatedCatalog } from "./catalog.mjs";
import { writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  mkdirSync,
  copyFileSync,
  chmodSync,
  existsSync,
  readFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { resolve } from "node:path";
const cargo =
  process.env.CARGO ||
  (existsSync(resolve(homedir(), ".cargo/bin/cargo"))
    ? resolve(homedir(), ".cargo/bin/cargo")
    : "cargo");
const workspaceVersion = readFileSync("../Cargo.toml", "utf8").match(
  /\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/,
)?.[1];
if (
  JSON.parse(readFileSync("package.json", "utf8")).version !== workspaceVersion
)
  throw new Error(
    "Desktop and Rust workspace versions must match before packaging.",
  );
const target =
  process.platform === "darwin" && process.arch === "arm64"
    ? "aarch64-apple-darwin"
    : process.platform === "linux" && process.arch === "x64"
      ? "x86_64-unknown-linux-gnu"
      : null;
if (!target)
  throw new Error(
    "Desktop releases are built on Apple Silicon macOS or x86_64 Linux. Use the platform CI job for the other target.",
  );
const result = spawnSync(
  cargo,
  [
    "build",
    "--release",
    "--locked",
    "--target",
    target,
    "-p",
    "twr-cli",
    "-p",
    "twr-app",
  ],
  { cwd: "..", stdio: "inherit" },
);
if (result.status !== 0) process.exit(result.status || 1);
const generated =
  JSON.stringify(
    await generatedCatalog(resolve(`../target/${target}/release/twr`)),
    null,
    2,
  ) + "\n";
if (readFileSync("mobile/catalog.json", "utf8") !== generated) {
  writeFileSync("mobile/catalog.json", generated);
  const rebuilt = spawnSync(
    cargo,
    ["build", "--release", "--locked", "--target", target, "-p", "twr-app"],
    { cwd: "..", stdio: "inherit" },
  );
  if (rebuilt.status !== 0) process.exit(rebuilt.status || 1);
}
mkdirSync("bin", { recursive: true });
// The packaged app names its engine directory from these digests instead of
// hashing ~70 MB on every launch, and verifies each copy against them.
const digests = {};
for (const name of ["twr", "twr-desktop-bridge", "twr-mobile-gateway"]) {
  copyFileSync(`../target/${target}/release/${name}`, `bin/${name}`);
  chmodSync(`bin/${name}`, 0o755);
  digests[name] = createHash("sha256")
    .update(readFileSync(`bin/${name}`))
    .digest("hex");
}
writeFileSync("bin/digests.json", JSON.stringify(digests, null, 2) + "\n");
