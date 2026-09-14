import assert from "node:assert/strict";
import { mcpTestText, parseEnvLines, splitMcpMethods } from "./mcp-model.ts";

assert.deepEqual(parseEnvLines("A=1\n\nB= two \nNOPE\n=skip\n"), { A: "1", B: "two" });
assert.deepEqual(parseEnvLines(""), {});
assert.deepEqual(splitMcpMethods("a, b,,c"), ["a", "b", "c"]);
assert.equal(mcpTestText(true, ["a", "b"]).text, "连通成功 · 2 个方法：a, b");
assert.equal(mcpTestText(true, []).text, "连通成功 · 0 个方法");
assert.equal(mcpTestText(false, [], "boom").text, "失败：boom");
const nine = mcpTestText(true, ["1", "2", "3", "4", "5", "6", "7", "8", "9"]).text;
assert.ok(nine.includes("1, 2, 3, 4, 5, 6, 7, 8"));
assert.ok(!nine.includes("1, 2, 3, 4, 5, 6, 7, 8, 9"));
assert.ok(nine.includes("…"));
