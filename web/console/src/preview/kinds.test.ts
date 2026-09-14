import assert from "node:assert/strict";
import { isOfficeKind, isOfficePath, kindFor, previewExt } from "./kinds.ts";
import {
  canSavePreview,
  maximizeLabel,
  officeDirtyNote,
  officeTick,
  previewDockClass,
  truncatedPreviewNote,
} from "./dock-model.ts";

assert.equal(previewExt("a/b.canvas.json"), ".canvas.json");
assert.equal(previewExt("x.PDF"), ".pdf");
assert.equal(previewExt(".pdf"), ".pdf");
assert.equal(previewExt("noext"), "");
assert.equal(kindFor("n.docx").id, "word");
assert.equal(kindFor("n.html").id, "browser");
assert.equal(kindFor("n.png").id, "image");
assert.equal(kindFor("n.xlsx").id, "sheet");
assert.equal(kindFor("n.pptx").id, "ppt");
assert.equal(kindFor("n.pdf").id, "pdf");
assert.equal(kindFor("n.canvas.json").id, "canvas");
assert.equal(kindFor("n.vsdx").id, "visio");
assert.equal(kindFor("n.rs").id, "text");
assert.equal(isOfficeKind("word"), true);
assert.equal(isOfficeKind("pdf"), true);
assert.equal(isOfficeKind("text"), false);
assert.equal(isOfficePath("a.md"), true);
assert.equal(isOfficePath("a.docx"), true);
assert.equal(isOfficePath("a.rs"), false);

assert.equal(canSavePreview(true, true, "k", false), true);
assert.equal(canSavePreview(true, true, "", true), false);
assert.equal(canSavePreview(true, false, "", true), true);
assert.equal(canSavePreview(false, false, "", true), false);
assert.equal(previewDockClass({ maximized: true, office: true, layout: "chat" }), "pv-dock max oo chat");
assert.equal(previewDockClass({ maximized: false, office: false, layout: "page" }), "pv-dock");
assert.deepEqual(officeTick({ ready: true }, false), { reload: true });
assert.deepEqual(officeTick({ ready: true }, true), { note: officeDirtyNote() });
assert.deepEqual(officeTick({ starting: true, hint: "boot" }, false), { note: "boot", wait: true });
assert.deepEqual(officeTick({ hint: "boot" }, false), { note: "boot", wait: false });
assert.deepEqual(officeTick({}, false), { note: undefined, wait: false });
assert.ok(truncatedPreviewNote(true).includes("96MB"));
assert.equal(truncatedPreviewNote(false), "");
assert.equal(maximizeLabel(true).aria, "还原预览");
assert.equal(maximizeLabel(true).title, "还原");
assert.equal(maximizeLabel(false).title, "最大化");
