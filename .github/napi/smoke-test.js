const path = require("path");
const fs = require("fs");

const [modulePath, pngPath, label] = process.argv.slice(2);

const { MediaProcessor } = require(path.resolve(modulePath));
const png = fs.readFileSync(path.resolve(pngPath));

(async () => {
  const p = new MediaProcessor();
  const avatar = await p.process(png, { avatar: true });
  if (!avatar.contentType.includes("webp")) throw new Error("avatar should be webp: " + avatar.contentType);
  const badge = await p.process(png, { badge: true });
  if (!badge.contentType.includes("png")) throw new Error("badge should be png: " + badge.contentType);
  console.log((label || "smoke") + " smoke test ok");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
