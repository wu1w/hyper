import assert from "node:assert/strict";
import { dirtyAncestors, nestTree, parseTreeEntry } from "./tree-model.ts";

{
  const e = parseTreeEntry({ path: "src/main.rs", name: "main.rs", dir: false });
  assert.equal(e?.path, "src/main.rs");
  assert.equal(e?.dir, false);
}

{
  const nodes = nestTree([
    { path: "src", name: "src", dir: true },
    { path: "src/main.rs", name: "main.rs", dir: false },
    { path: "Cargo.toml", name: "Cargo.toml", dir: false },
  ]);
  assert.equal(nodes.length, 2);
  const src = nodes.find((n) => n.path === "src");
  assert.equal(src?.children.length, 1);
  assert.equal(src?.children[0].path, "src/main.rs");
}

{
  const a = dirtyAncestors(["crates/hyper-loop/src/lib.rs"]);
  assert.ok(a.has("crates"));
  assert.ok(a.has("crates/hyper-loop"));
  assert.ok(a.has("crates/hyper-loop/src"));
  assert.equal(a.has("crates/hyper-loop/src/lib.rs"), false);
}
