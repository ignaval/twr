import sharp from "sharp";
import { readFileSync, writeFileSync } from "node:fs";
const svg = readFileSync("packaging/icon.svg");
for (const [density, size] of Object.entries({
  mdpi: 48,
  hdpi: 72,
  xhdpi: 96,
  xxhdpi: 144,
  xxxhdpi: 192,
})) {
  for (const name of ["ic_launcher", "ic_launcher_round"])
    await sharp(svg)
      .resize(size, size)
      .png()
      .toFile(`mobile/android/app/src/main/res/mipmap-${density}/${name}.png`);
  await sharp(svg)
    .resize(Math.round(size * 1.5), Math.round(size * 1.5))
    .extend({
      top: Math.round(size * 0.375),
      bottom: Math.round(size * 0.375),
      left: Math.round(size * 0.375),
      right: Math.round(size * 0.375),
      background: { r: 21, g: 27, b: 29, alpha: 1 },
    })
    .png()
    .toFile(
      `mobile/android/app/src/main/res/mipmap-${density}/ic_launcher_foreground.png`,
    );
}
await sharp(svg)
  .resize(1024, 1024)
  .flatten({ background: "#151b1d" })
  .png()
  .toFile(
    "mobile/ios/App/App/Assets.xcassets/AppIcon.appiconset/AppIcon-512@2x.png",
  );
writeFileSync(
  "mobile/android/app/src/main/res/values/ic_launcher_background.xml",
  '<?xml version="1.0" encoding="utf-8"?><resources><color name="ic_launcher_background">#151b1d</color></resources>\n',
);
