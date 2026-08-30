import assert from "node:assert/strict";
import { lastStableMdCut, parseMd, parseMdCached, type MdParseCache } from "./md-parse.ts";

function cache(): MdParseCache {
  return { prefix: "", blocks: [] };
}

function eqFull(src: string) {
  const c = cache();
  assert.deepEqual(parseMdCached(src, c), parseMd(src), src.slice(0, 80));
}

{
  assert.equal(lastStableMdCut("hello"), 0);
  assert.equal(lastStableMdCut("hello\nworld"), 0);
  const intro = "先改入口。\n\n";
  assert.equal(lastStableMdCut(intro), intro.length);
  assert.equal(lastStableMdCut(`${intro}\`\`\`rust\nfn send() {}`), intro.length);
  assert.equal(lastStableMdCut(`${intro}\`\`\`rust\nfn send() {}\nlet x = 1;`), intro.length);
  const closed = `${intro}\`\`\`rust\nfn send() {}\n\`\`\`\n\n`;
  assert.equal(lastStableMdCut(closed), closed.length);
  assert.equal(lastStableMdCut(`${closed}接下来写测试。`), closed.length);
}

{
  const intro = "先改入口。\n\n";
  const c = cache();
  const open = `${intro}\`\`\`rust\n`;
  parseMdCached(`${open}fn send() {}`, c);
  const prefix = c.prefix;
  const head = c.blocks;
  assert.equal(prefix, intro);
  assert.equal(head.length, 1);
  parseMdCached(`${open}fn send() {\n    let x = 1;\n}`, c);
  assert.equal(c.prefix, prefix);
  assert.equal(c.blocks, head);
  const closed = `${open}fn send() {}\n\`\`\`\n\n接下来。`;
  const after = parseMdCached(closed, c);
  assert.equal(c.prefix, `${open}fn send() {}\n\`\`\`\n\n`);
  assert.notEqual(c.blocks, head);
  assert.deepEqual(after, parseMd(closed));
}

{
  const samples = [
    "pong",
    "一段话没有空行",
    "第一段。\n\n第二段。",
    "标题\n\n```js\nconst x = 1\n```\n\n收束。",
    "# 头\n\n- a\n- b\n\n> 引用\n\n```\ncode\n```",
    "```rust\nunclosed",
    "para\n\n```\nstill open\nlet x = 1",
  ];
  for (const src of samples) eqFull(src);

  const stream = "定位 send。\n\n```rust\npub async fn send() {\n    Ok(())\n}\n```\n\n已写好。";
  const c = cache();
  let built = "";
  for (const ch of stream) {
    built += ch;
    const got = parseMdCached(built, c);
    assert.deepEqual(got, parseMd(built), `stream at ${built.length}`);
  }
}
