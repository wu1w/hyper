import assert from "node:assert/strict";
import { fileHref, isImageMedia, isImagePath, mediaSrc, parseStoredMedia, pathFromMediaUrl, siteHref } from "./media.ts";

assert.equal(isImagePath("a.png"), true);
assert.equal(isImagePath("a.PNG"), true);
assert.equal(isImagePath("a.rs"), false);
assert.equal(isImagePath("a.png?x=1"), true);
assert.equal(isImageMedia({ kind: "image" }), true);
assert.equal(isImageMedia({ mime: "image/png" }), true);
assert.equal(isImageMedia({ url: "out/a.png" }), true);
assert.equal(isImageMedia({ kind: "", mime: "", url: "a.rs" }), false);
assert.equal(mediaSrc(""), "");
assert.equal(mediaSrc("https://x/a.png"), "https://x/a.png");
assert.equal(mediaSrc("/api/files?path=a"), "/api/files?path=a");
assert.ok(mediaSrc("out/a.png").includes("/api/files?path="));
assert.equal(fileHref("a.ts"), "/api/files?path=a.ts");
assert.equal(fileHref("a.ts", true), "/api/files?path=a.ts&dl=1");
assert.equal(siteHref("out/demo/index.html"), "/api/raw/out/demo/index.html");
assert.equal(siteHref("out/./demo/index.html"), "/api/raw/out/demo/index.html");
assert.ok(siteHref("out/x", 3).includes("?v=3"));
assert.equal(pathFromMediaUrl("https://x"), "");
assert.equal(pathFromMediaUrl("/api/files?path=out%2Fa.png"), "out/a.png");
assert.equal(pathFromMediaUrl("out/a.png"), "out/a.png");
assert.deepEqual(parseStoredMedia([{ url: "a.png", kind: "image" }]), [
  { kind: "image", mime: "", url: "a.png" },
]);
assert.deepEqual(parseStoredMedia([null, "x", { url: "" }]), []);
