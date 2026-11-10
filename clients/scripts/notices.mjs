// Writes dist-notices/THIRD-PARTY-LICENSES.html, the third-party notices every desktop
// package carries (package.json extraResources; Help › Third-Party Licences opens it):
//
// - the Rust crates in the bundled engines (twr, twr-desktop-bridge, twr-mobile-gateway),
//   from cargo-about with the CLI release's settings (about.toml, about.hbs), and
// - the npm packages in the app: those bundled into its interface and main process, and the
//   production dependencies electron-builder copies into it.
//
// Electron and Chromium ship their own notices (LICENSE.electron.txt, LICENSES.chromium.html).
// Without cargo-about this fails, unless TWR_NOTICES_WITHOUT_RUST=1, which writes a file that
// says on top that it lacks the Rust crates (for local review builds only).
import { build } from "esbuild";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const clients = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const root = resolve(clients, "..");
const output = join(clients, "dist-notices", "THIRD-PARTY-LICENSES.html");
const CARGO_ABOUT_VERSION = "0.9.2";

const escape = (text) =>
  String(text)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

/** The package directory (relative to clients/) an input path belongs to. */
export function packageDirectory(path) {
  const parts = path.split("/");
  const last = parts.lastIndexOf("node_modules");
  if (last < 0 || last + 1 >= parts.length) return undefined;
  const length = parts[last + 1].startsWith("@") ? 2 : 1;
  return parts.slice(0, last + 1 + length).join("/");
}

/** Package directories esbuild pulls into a bundle of `entryPoints`. */
async function bundled(options) {
  const result = await build({
    bundle: true,
    write: false,
    metafile: true,
    logLevel: "silent",
    absWorkingDir: clients,
    outdir: "notices-scratch",
    ...options,
  });
  const directories = new Set();
  for (const input of Object.keys(result.metafile.inputs)) {
    const directory = packageDirectory(input);
    if (directory) directories.add(directory);
  }
  return directories;
}

/** What electron-builder copies into the app: the lockfile's non-development packages. */
function shipped() {
  const lock = JSON.parse(
    readFileSync(join(clients, "package-lock.json"), "utf8"),
  );
  const directories = new Set();
  for (const [path, entry] of Object.entries(lock.packages || {}))
    if (path && !entry.dev && !entry.link && existsSync(join(clients, path)))
      directories.add(path);
  return directories;
}

function describePackage(directory) {
  const manifest = JSON.parse(
    readFileSync(join(clients, directory, "package.json"), "utf8"),
  );
  const license =
    typeof manifest.license === "string"
      ? manifest.license
      : manifest.license?.type ||
        (Array.isArray(manifest.licenses)
          ? manifest.licenses.map((l) => l.type || l).join(" OR ")
          : "UNKNOWN");
  const repository =
    typeof manifest.repository === "string"
      ? manifest.repository
      : manifest.repository?.url;
  const url = (repository || manifest.homepage || "")
    .replace(/^git\+/, "")
    .replace(/^git:\/\//, "https://")
    .replace(/\.git$/, "");
  const files = readdirSync(join(clients, directory))
    .filter((name) => /^(licen[cs]e|copying|notice)/i.test(name))
    .sort()
    .map((name) => ({
      name,
      text: readFileSync(join(clients, directory, name), "utf8"),
    }));
  return {
    name: manifest.name,
    version: manifest.version,
    license,
    url: /^https:\/\//.test(url)
      ? url
      : `https://www.npmjs.com/package/${manifest.name}`,
    files,
  };
}

async function npmPackages() {
  const host = await bundled({
    entryPoints: ["desktop/main.ts", "desktop/preload.ts"],
    platform: "node",
    format: "cjs",
    external: ["electron", "node-pty"],
  });
  const renderer = await bundled({
    entryPoints: ["desktop/renderer.tsx"],
    platform: "browser",
    format: "esm",
    jsx: "automatic",
    loader: {
      ".png": "empty",
      ".svg": "empty",
      ".woff": "empty",
      ".woff2": "empty",
      ".ttf": "empty",
    },
  });
  const directories = new Set([...host, ...renderer, ...shipped()]);
  const packages = new Map();
  for (const directory of directories) {
    const described = describePackage(directory);
    // One entry per name and version, however many copies node_modules holds.
    packages.set(`${described.name}@${described.version}`, described);
  }
  return [...packages.values()].sort((a, b) =>
    a.name < b.name ? -1 : a.name > b.name ? 1 : 0,
  );
}

function npmSection(packages) {
  const items = packages.map((p) => {
    const texts = p.files.length
      ? p.files
          .map(
            (file) =>
              `<p>${escape(file.name)}:</p>\n<pre class="license-text">${escape(file.text)}</pre>`,
          )
          .join("\n")
      : `<p>The package has no licence file; its package.json gives the licence as ${escape(p.license)}.</p>`;
    return `<li data-package="${escape(p.name)}">
<h3 id="npm-${escape(p.name.replace(/[^A-Za-z0-9-]/g, "_"))}"><a href="${escape(p.url)}">${escape(p.name)} ${escape(p.version)}</a></h3>
<p>Licence: ${escape(p.license)}</p>
${texts}
</li>`;
  });
  return `<h2 id="npm-packages">npm packages</h2>
<p>The app's interface and main process contain these npm packages, under the licences and
notices below.</p>
<ul class="licenses-list">
${items.join("\n")}
</ul>
`;
}

const INTRO = `<p>twr is licensed under the Apache License 2.0 (<a href="LICENSE">LICENSE</a> and
<a href="NOTICE">NOTICE</a>, next to this file). The twr desktop app also contains open source
code from others: the Rust crates built into its bundled engines (twr, twr-desktop-bridge and
twr-mobile-gateway), then the <a href="#npm-packages">npm packages</a> of its interface and
main process. Electron and Chromium, which run the app, list their own licences in
LICENSE.electron.txt and LICENSES.chromium.html in the app.</p>`;

function cargoAbout() {
  const cargo =
    process.env.CARGO ||
    (existsSync(join(homedir(), ".cargo/bin/cargo"))
      ? join(homedir(), ".cargo/bin/cargo")
      : "cargo");
  const version = spawnSync(cargo, ["about", "--version"], {
    cwd: root,
    encoding: "utf8",
  });
  if (version.status !== 0) return undefined;
  // The CLI release's command (release.yml), to stdout.
  const result = spawnSync(
    cargo,
    ["about", "generate", "--locked", "--fail", "about.hbs"],
    { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  );
  if (result.status !== 0) {
    process.stderr.write(result.stderr || "");
    throw new Error("cargo about generate failed (see above)");
  }
  return result.stdout;
}

const packages = await npmPackages();
const npm = npmSection(packages);
let html;
const rust = cargoAbout();
if (rust) {
  // about.hbs's page, its introduction replaced and the npm packages added.
  const intro = /<h1>Third-party licences<\/h1>\s*<p>[\s\S]*?<\/p>/;
  if (!intro.test(rust) || !rust.includes("</main>"))
    throw new Error("about.hbs changed: update scripts/notices.mjs");
  html = rust
    .replace(
      "<title>twr: third-party licences</title>",
      "<title>twr desktop: third-party licences</title>",
    )
    .replace(
      intro,
      `<h1>Third-party licences</h1>\n${INTRO}\n<h2 id="rust-crates">Rust crates</h2>`,
    )
    .replace("</main>", `${npm}</main>`);
} else if (process.env.TWR_NOTICES_WITHOUT_RUST === "1") {
  console.warn(
    "WARNING: cargo-about is not installed; THIRD-PARTY-LICENSES.html lists only the npm packages and says so. Do not distribute this build.",
  );
  html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>twr desktop: third-party licences (incomplete)</title>
<style>
@media (prefers-color-scheme: dark) { body { background: #222; color: #eee; } a { color: skyblue; } }
main { font-family: sans-serif; max-width: 800px; margin: 0 auto; }
.licenses-list { list-style-type: none; margin: 0; padding: 0; }
.license-text { max-height: 200px; overflow-y: scroll; white-space: pre-wrap; }
.incomplete { border: 2px solid #c00; padding: 0.5em 1em; }
</style>
</head>
<body>
<main>
<h1>Third-party licences</h1>
<p class="incomplete"><strong>Incomplete:</strong> this local build was packaged without
cargo-about, so the Rust crates in its engines are not listed. It is not for distribution.</p>
${INTRO}
${npm}</main>
</body>
</html>
`;
} else {
  console.error(`The desktop packages carry the licences of the Rust crates in their engines, which cargo-about writes.
Install it (the release workflow uses ${CARGO_ABOUT_VERSION}):

  cargo install --locked cargo-about --version ${CARGO_ABOUT_VERSION}

or, for a local review build that will not be distributed, package without them:

  TWR_NOTICES_WITHOUT_RUST=1 npm run package:mac`);
  process.exit(1);
}
mkdirSync(dirname(output), { recursive: true });
writeFileSync(output, html);
const missing = packages.filter((p) => !p.files.length);
console.log(
  `Wrote ${relative(clients, output)}: ${rust ? "Rust crates and " : ""}${packages.length} npm packages` +
    (missing.length
      ? ` (${missing.length} without a licence file: ${missing.map((p) => p.name).join(", ")})`
      : ""),
);
