// Generates the status-area icons in packaging/tray from the application mark.
// macOS uses alpha-only template images; Linux status notifiers show color.
import sharp from "sharp";
import { readFileSync, mkdirSync } from "node:fs";
const out = "packaging/tray";
mkdirSync(out, { recursive: true });
const glyph = readFileSync("packaging/icon.svg", "utf8").match(
  /<path d="(M171 [^"]+)"/,
)[1];
// The mark at status-area sizes: a rounded square with the "t" and the cursor
// bar knocked out, so it stays legible at 16 pt.
const template = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">
<defs><mask id="m"><rect width="1024" height="1024" fill="#fff"/>
<g transform="translate(-6 -229) scale(1.5)"><path d="${glyph}" fill="#000"/></g>
<rect x="510" y="712" width="290" height="92" rx="20" fill="#000"/></mask></defs>
<rect x="40" y="40" width="944" height="944" rx="230" fill="#000" mask="url(#m)"/></svg>`;
const color = (
  dot,
) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">
<defs><mask id="m"><rect width="1024" height="1024" fill="#fff"/>
${dot ? '<circle cx="840" cy="184" r="250" fill="#000"/>' : ""}</mask></defs>
<g mask="url(#m)"><rect x="40" y="40" width="944" height="944" rx="230" fill="#151b1d" stroke="#35463f" stroke-width="24"/>
<g transform="translate(-6 -229) scale(1.5)"><path d="${glyph}" fill="#e5eee8"/></g>
<rect x="510" y="712" width="290" height="92" rx="20" fill="#8de3cb"/></g>
${dot ? '<circle cx="840" cy="184" r="184" fill="#e0b45c"/>' : ""}</svg>`;
const render = (svg, size, file) =>
  sharp(Buffer.from(svg), { density: 300 })
    .resize(size, size)
    .png()
    .toFile(`${out}/${file}`);
await render(template, 16, "trayTemplate.png");
await render(template, 32, "trayTemplate@2x.png");
await render(color(false), 64, "tray.png");
await render(color(true), 64, "tray-attention.png");
