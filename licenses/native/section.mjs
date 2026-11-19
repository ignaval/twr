// The C and C++ libraries of twr's own voices (components.json, the licence texts next to
// it) as an HTML section of THIRD-PARTY-LICENSES.html: cargo-about lists only the Rust
// crates, and sherpa-onnx-sys links these from its prebuilt static libraries.
//
//   node licenses/native/section.mjs THIRD-PARTY-LICENSES.html   # adds the section in place
//
// clients/scripts/notices.mjs (the desktop app) imports `nativeSection`. A release built
// without the `neural` feature (the static Linux binaries) contains none of them, and its
// workflow doesn't add the section.
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

const escape = (s) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** The section, every component with its licence texts. */
export function nativeSection(dir = here) {
  const { components } = JSON.parse(readFileSync(join(dir, "components.json"), "utf8"));
  const items = components.map((c) => {
    const texts = c.files
      .map((f) => {
        const text = readFileSync(join(dir, f), "utf8").replace(/^﻿/, "");
        return `<p>${escape(f)}:</p>\n<pre class="license-text">${escape(text)}</pre>`;
      })
      .join("\n");
    const note = c.note ? `\n<p>${escape(c.note)}</p>` : "";
    return `<li>
<h3>${escape(c.name)} ${escape(c.version)}</h3>
<p>Licence: ${escape(c.license)}. Source: <a href="${escape(c.url)}">${escape(c.url)}</a></p>${note}
${texts}
</li>`;
  });
  return `<h2 id="native-libraries">Native libraries (twr's own voices)</h2>
<p>twr's own voices (<code>twr voices</code>) run on sherpa-onnx, which twr links statically
with these C and C++ libraries, under the licences below.</p>
<ul class="licenses-list">
${items.join("\n")}
</ul>
`;
}

/** `html` (a THIRD-PARTY-LICENSES.html) with the section added before its `</main>`. */
export function withNativeSection(html, dir = here) {
  if (!html.includes("</main>")) throw new Error("no </main> to add the native libraries before");
  if (html.includes('id="native-libraries"')) return html;
  return html.replace("</main>", `${nativeSection(dir)}</main>`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const file = process.argv[2];
  if (!file) {
    console.error("usage: node licenses/native/section.mjs THIRD-PARTY-LICENSES.html");
    process.exit(2);
  }
  writeFileSync(file, withNativeSection(readFileSync(file, "utf8")));
}
