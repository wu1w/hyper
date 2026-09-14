import assert from "node:assert/strict";
import {
  headingPr,
  htmlEscape,
  legacyWordNote,
  mergeRunStyle,
  textAsHtml,
  wPara,
  wRun,
  wText,
  wrapHtmlDocument,
  xmlEscape,
  xmlTextFallback,
} from "./word-model.ts";

assert.equal(xmlEscape("a&b<c>"), "a&amp;b&lt;c&gt;");
assert.equal(htmlEscape("<x>"), "&lt;x&gt;");
assert.equal(xmlTextFallback("<w:p><w:t>hi</w:t></w:p><w:br/>"), "hi");
assert.equal(xmlTextFallback("x<w:br/>y"), "x\ny");
assert.ok(xmlTextFallback("<w:p>a</w:p><w:p>b</w:p>").includes("\n"));
assert.equal(xmlTextFallback("a<w:tab/>b").includes("\t"), true);
assert.equal(textAsHtml("a\n"), "<p>a</p><p><br></p>");
assert.ok(wrapHtmlDocument("x").includes("<body>x</body>"));
assert.equal(wText(""), "");
assert.ok(wText("hi").includes("hi"));
assert.ok(wText("&").includes("&amp;"));
assert.equal(wRun(""), "");
assert.ok(wRun("x", { bold: true }).includes("<w:b/>"));
assert.ok(wRun("x", { italic: true }).includes("<w:i/>"));
assert.ok(wRun("x", { hyper: true }).includes("0563C1"));
assert.ok(wRun("x", { underline: true }).includes('w:val="single"'));
assert.deepEqual(mergeRunStyle({ bold: true }, { italic: true }), {
  bold: true,
  italic: true,
  underline: undefined,
  hyper: undefined,
});
assert.equal(mergeRunStyle({ underline: true }, {}).underline, true);
assert.equal(mergeRunStyle({}, { underline: true }).underline, true);
assert.ok(wPara("x", "<w:ind/>").includes("<w:pPr>"));
assert.ok(wPara("", "").includes("<w:r/>"));
assert.ok(headingPr("h1").includes('w:val="36"'));
assert.ok(headingPr("h2").includes('w:val="32"'));
assert.ok(legacyWordNote("a.doc", new Uint8Array([1]))?.includes("旧版 .doc"));
assert.ok(legacyWordNote("a.odt", new Uint8Array([1]))?.includes("OpenDocument"));
assert.ok(legacyWordNote("a.docx", new Uint8Array([1, 2, 3]))?.includes("不是有效的 docx"));
assert.equal(legacyWordNote("a.docx", new Uint8Array([0x50, 0x4b, 0, 0])), null);
assert.ok(legacyWordNote("a.docx", new Uint8Array([0xd0, 0xcf, 0x11, 0xe0, 0, 0, 0, 0]))?.includes("二进制"));
