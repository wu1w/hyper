import assert from "node:assert/strict";
import { looksLikeOle, looksLikePdf, looksLikeZip, mimeFromPath } from "./bytes.ts";

assert.equal(looksLikeZip(new Uint8Array([0x50, 0x4b, 0, 0])), true);
assert.equal(looksLikeZip(new Uint8Array([0x50, 0x4b])), false);
assert.equal(looksLikeZip(new Uint8Array([1, 2, 3, 4])), false);
assert.equal(looksLikeOle(new Uint8Array([0xd0, 0xcf, 0x11, 0xe0, 0, 0, 0, 0])), true);
assert.equal(looksLikeOle(new Uint8Array([0xd0, 0xcf, 0x11, 0xe0])), false);
assert.equal(looksLikePdf(new Uint8Array([0x25, 0x50, 0x44, 0x46, 0x2d])), true);
assert.equal(looksLikePdf(new Uint8Array([0x25, 0x50, 0x44, 0x46])), false);
assert.equal(mimeFromPath("a.PNG"), "image/png");
assert.equal(mimeFromPath(".png"), "image/png");
assert.equal(mimeFromPath("a.pdf"), "application/pdf");
assert.equal(mimeFromPath("a.docx"), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
assert.equal(mimeFromPath("a.bin"), "application/octet-stream");
