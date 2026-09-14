import assert from "node:assert/strict";
import { parseWsRecents, pushRecentPath, recentLabel, treePadLeft, filesLocked, openEntryAction } from "./files-model.ts";

assert.deepEqual(parseWsRecents(null), []);
assert.deepEqual(parseWsRecents("not-json"), []);
assert.deepEqual(parseWsRecents('["/a","/b"]'), ["/a", "/b"]);
assert.deepEqual(parseWsRecents('["", 1, "/ok"]'), ["/ok"]);

assert.deepEqual(pushRecentPath("/new", ["/a", "/b"], 8), ["/new", "/a", "/b"]);
assert.deepEqual(pushRecentPath("/a", ["/a", "/b"], 8), ["/a", "/b"]);
assert.deepEqual(pushRecentPath("  ", ["/a"], 8), ["/a"]);
assert.equal(pushRecentPath("/c", ["/1", "/2", "/3"], 3).length, 3);
assert.equal(pushRecentPath("/c", ["/1", "/2", "/3"], 3)[0], "/c");

assert.equal(recentLabel("/Users/me/proj"), "proj");
assert.equal(recentLabel("C\\\\Users\\\\me"), "me");
assert.equal(recentLabel("/"), "/");
assert.equal(treePadLeft("a"), 8);
assert.equal(treePadLeft("a/b/c"), 8 + 24);
assert.equal(treePadLeft("a/b"), 8 + 12);
assert.equal(filesLocked(true, false), true);
assert.equal(filesLocked(false, true), true);
assert.equal(filesLocked(false, false), false);
assert.equal(openEntryAction(true, true), "noop");
assert.equal(openEntryAction(true, false), "workspace");
assert.equal(openEntryAction(false, true), "preview");
assert.equal(openEntryAction(false, false), "preview");
