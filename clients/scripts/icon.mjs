import sharp from "sharp";
await sharp("packaging/icon.svg").png().toFile("packaging/icon.png");
