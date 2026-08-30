import assert from "node:assert/strict";
import {
  isOutPath,
  mergeArtifactLists,
  siblingStamp,
  turnArtifacts,
  turnEditedPaths,
  turnPreviewPaths,
  turnTouchedPaths,
} from "./artifacts.ts";
import { siteHref } from "./media.ts";
import type { SessionEvent } from "./api.ts";

const WS = "/tmp/hyper-dialect-ws";

function tool(name: string, args: unknown, output: string, extra?: Partial<SessionEvent>): SessionEvent[] {
  return [
    {
      type: "assistant",
      tool_calls: [{ function: { name, arguments: JSON.stringify(args) } }],
    },
    { type: "tool", name, output, ...extra },
  ];
}

function names(events: SessionEvent[]) {
  return turnArtifacts(events, WS);
}

function turn(user: string, rest: SessionEvent[]) {
  return names([{ type: "user", text: user }, ...rest]);
}

{
  assert.equal(isOutPath("out/guide.docx"), true);
  assert.equal(isOutPath("out/demo/index.html"), true);
  assert.equal(isOutPath(`${WS}/out/guide.docx`), true);
  assert.equal(isOutPath("out.pptx"), false);
  assert.equal(isOutPath("notes/out.md"), false);
  assert.equal(isOutPath("timeout/x.docx"), false);
}

{
  assert.deepEqual(turn("现在几点", []), []);
}

{
  const events: SessionEvent[] = [
    { type: "user", text: "给你自己做一个使用说明的word文档" },
    ...tool(
      "Write",
      {
        path: `${WS}/build_user_guide.py`,
        contents: 'out = "/tmp/hyper-dialect-ws/out/grok-hyper-使用说明.docx"\n',
      },
      "Wrote 38213 bytes to /tmp/hyper-dialect-ws/build_user_guide.py.",
    ),
    ...tool(
      "Shell",
      {
        command: `${WS}/.pptx-venv/bin/python ${WS}/build_user_guide.py && ls -la "${WS}/out/grok-hyper-使用说明.docx"`,
      },
      `${WS}/out/grok-hyper-使用说明.docx\n-rw-r--r--@ 1 william  wheel  52041 Aug 25 19:51 ${WS}/out/grok-hyper-使用说明.docx\n`,
    ),
    { type: "assistant", content: "已写好使用说明 Word，路径是 `out/grok-hyper-使用说明.docx`。" },
  ];
  assert.deepEqual(turnArtifacts(events, WS), ["out/grok-hyper-使用说明.docx"]);
}

{
  assert.deepEqual(
    turn("做 ppt", tool("Shell", { command: `python ${WS}/build_self_intro.py` }, `${WS}/out/grok-hyper-self-intro.pptx\n`)),
    ["out/grok-hyper-self-intro.pptx"],
  );
}

{
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/build_user_guide.py`, contents: "print(1)\n" }, "Wrote 8 bytes to build_user_guide.py.")),
    [],
  );
}

{
  const events: SessionEvent[] = [
    { type: "user", text: "改标题" },
    ...tool(
      "StrReplace",
      { path: `${WS}/src/App.tsx`, old_string: "a", new_string: "b" },
      "Successfully replaced text in src/App.tsx.",
    ),
  ];
  assert.deepEqual(turnEditedPaths(events, WS), ["src/App.tsx"]);
  assert.deepEqual(turnArtifacts(events, WS), []);
}

{
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/guide.docx`, contents: "x" }, `Wrote 1 bytes to ${WS}/guide.docx.`)),
    [],
  );
}

{
  assert.deepEqual(
    turn("x", tool("Shell", { command: "unzip -l out.docx" }, "word/document.xml\nword/media/image1.png\nppt/slides/slide1.xml\n")),
    [],
  );
}

{
  assert.deepEqual(
    turn("x", [{ type: "assistant", content: "打开 `notes/out.md` 即可。" }]),
    [],
  );
}

const WRITE_FILES = [
  "deck.pptx",
  "legacy.ppt",
  "guide.docx",
  "legacy.doc",
  "note.odt",
  "memo.rtf",
  "table.xlsx",
  "legacy.xls",
  "grid.ods",
  "slides.odp",
  "one.pdf",
  "page.html",
  "page.htm",
  "dot.png",
  "photo.jpg",
  "icon.svg",
  "table.csv",
  "diagram.vsdx",
  "legacy.vsd",
  "board.canvas.json",
];
for (const file of WRITE_FILES) {
  const got = turn("做文件", tool("Write", { path: `${WS}/out/${file}`, contents: "x" }, `Wrote 1 bytes to ${WS}/out/${file}.`));
  assert.deepEqual(got, [`out/${file}`], `Write out/${file} -> ${got.join(",")}`);
}

{
  const cases: Array<{ file: string; cmd: string; out: string }> = [
    { file: "out/deck.pptx", cmd: `python build.py && echo ${WS}/out/deck.pptx`, out: `${WS}/out/deck.pptx\n` },
    { file: "out/note.docx", cmd: `python -c 'doc.save("${WS}/out/note.docx")'`, out: `${WS}/out/note.docx\n` },
    { file: "out/table.xlsx", cmd: `python -c 'df.to_excel("${WS}/out/table.xlsx")'`, out: `${WS}/out/table.xlsx\n` },
    { file: "out/grid.csv", cmd: `python -c 'df.to_csv("${WS}/out/grid.csv")'`, out: `${WS}/out/grid.csv\n` },
    { file: "out/page.html", cmd: `python -c 'df.to_html("${WS}/out/page.html")'`, out: `${WS}/out/page.html\n` },
    { file: "out/chart.png", cmd: `python -c 'plt.savefig("${WS}/out/chart.png")'`, out: `${WS}/out/chart.png\n` },
    { file: "out/one.pdf", cmd: `python -c 'c.save("${WS}/out/one.pdf")'`, out: `${WS}/out/one.pdf\n` },
  ];
  for (const { file, cmd, out } of cases) {
    const got = turn("生成", tool("Shell", { command: cmd }, out));
    assert.ok(got.includes(file), `shell ${file} -> ${got.join(",")}`);
  }
}

{
  assert.deepEqual(
    turn(
      "生成",
      tool("Write", { path: `${WS}/build.py`, contents: 'doc.save("out.docx")\n' }, "Wrote 20 bytes to build.py."),
    ),
    [],
  );
}

{
  assert.deepEqual(
    turn("x", [{ type: "assistant", content: "成品在 `report.docx` 和 `deck.pptx`。" }]),
    [],
  );
}

{
  assert.deepEqual(
    turn(
      "做幻灯片",
      tool(
        "Shell",
        { command: `python3 scripts/build_pptx.py outline.json -o ${WS}/out/pitch.pptx` },
        "wrote pitch.pptx\n",
      ),
    ),
    ["out/pitch.pptx"],
  );
}

{
  assert.deepEqual(
    turn(
      "做表",
      tool(
        "Shell",
        { command: `python build.py && ls -la "${WS}/out/Q3 报告.xlsx"` },
        `-rw-r-- 1 u  wheel  12 Aug 25 20:00 ${WS}/out/Q3 报告.xlsx\n`,
      ),
    ),
    ["out/Q3 报告.xlsx"],
  );
}

{
  const got = turn(
    "转pdf",
    tool(
      "Shell",
      { command: `soffice --headless --convert-to pdf --outdir ${WS}/out ${WS}/out/deck.pptx` },
      `convert ${WS}/out/deck.pptx as a PDF document -> ${WS}/out/deck.pdf using filter : writer_pdf_Export\n`,
    ),
  );
  assert.deepEqual(got, ["out/deck.pptx", "out/deck.pdf"]);
}

{
  const got = turn("拷贝", tool("Shell", { command: `cp ${WS}/tmp.pptx ${WS}/out/final.pptx` }, ""));
  assert.ok(got.includes("out/final.pptx"), String(got));
}

{
  assert.deepEqual(
    turn("写", tool("Write", { path: `${WS}/out/word/guide.docx`, contents: "x" }, `Wrote 1 bytes to ${WS}/out/word/guide.docx.`)),
    ["out/word/guide.docx"],
  );
}

{
  const events: SessionEvent[] = [
    { type: "user", text: "画一只猫" },
    {
      type: "assistant",
      content: "好了",
      media: [{ kind: "image", mime: "image/jpeg", url: ".grok-hyper/generated/imagine-abcd.jpg" }],
    },
  ];
  assert.deepEqual(turnArtifacts(events, WS), [".grok-hyper/generated/imagine-abcd.jpg"]);
}

{
  const events: SessionEvent[] = [
    { type: "user", text: "看看这个\n[attached: .grok-hyper/uploads/scan.pdf]" },
  ];
  assert.deepEqual(turnArtifacts(events, WS), []);
  assert.deepEqual(turnPreviewPaths(events, WS), [".grok-hyper/uploads/scan.pdf"]);
}

{
  assert.deepEqual(
    turn("x", [{ type: "assistant", content: "见 `https://example.com/file.pdf`" }]),
    [],
  );
}

{
  const ls = `-rw-r--r--@ 1 william  wheel  3468485 Aug 25 17:33 HLX10-002-NSCLC301-CSR-v3-TOC-fixed.docx
-rw-r--r--@ 1 william  wheel    39340 Aug 25 16:55 grok-hyper-self-intro.pptx
`;
  assert.deepEqual(turn("x", tool("Shell", { command: "ls -la" }, ls)), []);
}

{
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/dist/bundle.html`, contents: "<p/>" }, "Wrote 4 bytes to dist/bundle.html.")),
    [],
  );
}

{
  const cmd = `cd /var/www/onlyoffice/documentserver/server/FileConverter/bin && ./x2t /tmp/guide.docx /tmp/guide.pdf ./font_selection.bin; echo exit:$?; ls -la /tmp/guide.pdf`;
  assert.deepEqual(turn("转pdf", tool("Shell", { command: cmd }, `${cmd}\n`)), []);
  assert.deepEqual(
    turn("x", tool("Shell", { command: `DOC="${WS}/grok-hyper-使用说明.docx"` }, "")),
    [],
  );
}

{
  assert.deepEqual(
    turn("记一下", tool("Write", { path: `${WS}/notes/out.md`, contents: "hi" }, "Wrote 2 bytes to notes/out.md.")),
    [],
  );
}

{
  const events: SessionEvent[] = [
    { type: "user", text: "做网页demo" },
    ...tool("Write", { path: `${WS}/out/mw-csr-demo/index.html`, contents: "<link href=./styles.css>" }, `Wrote 80 bytes to ${WS}/out/mw-csr-demo/index.html.`),
    ...tool("Write", { path: `${WS}/out/mw-csr-demo/app.js`, contents: "1" }, `Wrote 1 bytes to ${WS}/out/mw-csr-demo/app.js.`),
    ...tool("Write", { path: `${WS}/out/mw-csr-demo/styles.css`, contents: "body{}" }, `Wrote 8 bytes to ${WS}/out/mw-csr-demo/styles.css.`),
  ];
  assert.deepEqual(turnArtifacts(events, WS), ["out/mw-csr-demo/index.html"]);
  const touched = turnTouchedPaths(events, WS);
  assert.ok(touched.includes("out/mw-csr-demo/index.html"), String(touched));
  assert.ok(touched.includes("out/mw-csr-demo/app.js"), String(touched));
  assert.ok(touched.includes("out/mw-csr-demo/styles.css"), String(touched));
  const stamp = siblingStamp("out/mw-csr-demo/index.html", touched);
  assert.ok(stamp.includes("app.js") && stamp.includes("styles.css"), stamp);
}

{
  assert.deepEqual(
    mergeArtifactLists(["out/b.html"], ["out/a.pptx"]),
    ["out/a.pptx", "out/b.html"],
  );
}

{
  assert.equal(siteHref("out/mw-csr-demo/index.html"), "/api/raw/out/mw-csr-demo/index.html");
  const page = new URL("http://127.0.0.1:3848/api/raw/out/mw-csr-demo/index.html");
  assert.equal(new URL("./app.js", page).pathname, "/api/raw/out/mw-csr-demo/app.js");
}

console.log("artifacts.test.ts ok");
