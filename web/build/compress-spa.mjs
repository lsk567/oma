/**
 * Gzips the SPA bundle for `omar serve --ui` to embed.
 *
 * The runtime stores these bytes inside the binary and hands them out with
 * `Content-Encoding: gzip`, so compressing once here is what keeps the binary
 * from growing by the full 2.2MB. Downloads are gzipped either way; this is
 * about what sits on disk.
 */
import { gzipSync } from "node:zlib";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// The exact list the runtime embeds. Adding one here means adding it there:
// both sides name the files, so a mismatch is a compile error rather than a
// 404 someone finds in a browser.
const FILES = ["index.html", "app.js", "app.css", "omar-logo.png", "favicon.svg"];
const dir = new URL("../dist/spa/", import.meta.url).pathname;

let raw = 0;
let packed = 0;
for (const name of FILES) {
  const source = readFileSync(join(dir, name));
  const gzipped = gzipSync(source, { level: 9 });
  writeFileSync(join(dir, `${name}.gz`), gzipped);
  raw += source.length;
  packed += gzipped.length;
}

const kb = (bytes) => `${Math.round(bytes / 1024)}KB`;
console.log(`compressed ${FILES.length} files: ${kb(raw)} -> ${kb(packed)}`);
