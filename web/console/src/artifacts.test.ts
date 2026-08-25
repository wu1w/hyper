import assert from "node:assert/strict";
import { turnArtifacts, turnPreviewPaths } from "./artifacts";
import type { SessionEvent } from "./api";

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
  const events: SessionEvent[] = [
    { type: "user", text: "给你自己做一个使用说明的word文档" },
    ...tool(
      "Write",
      {
        path: `${WS}/build_user_guide.py`,
        contents: 'out = "/tmp/hyper-dialect-ws/grok-hyper-使用说明.docx"\n',
      },
      "Wrote 38213 bytes to /tmp/hyper-dialect-ws/build_user_guide.py.",
    ),
    ...tool(
      "Shell",
      {
        command: `${WS}/.pptx-venv/bin/python ${WS}/build_user_guide.py && ls -la "${WS}/grok-hyper-使用说明.docx"`,
      },
      `${WS}/grok-hyper-使用说明.docx\n-rw-r--r--@ 1 william  wheel  52041 Aug 25 19:51 ${WS}/grok-hyper-使用说明.docx\n`,
    ),
    { type: "assistant", content: "已写好使用说明 Word，路径是 `grok-hyper-使用说明.docx`。" },
  ];
  assert.deepEqual(turnArtifacts(events, WS), ["grok-hyper-使用说明.docx"]);
}

{
  assert.deepEqual(
    turn("做 ppt", tool("Shell", { command: `python ${WS}/build_self_intro.py` }, `${WS}/grok-hyper-self-intro.pptx\n`)),
    ["grok-hyper-self-intro.pptx"],
  );
}

{
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/build_user_guide.py`, contents: "print(1)\n" }, "Wrote 8 bytes to build_user_guide.py.")),
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

// Direct Write of each preview-kind product.
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
  const got = turn("做文件", tool("Write", { path: `${WS}/${file}`, contents: "x" }, `Wrote 1 bytes to ${WS}/${file}.`));
  assert.deepEqual(got, [file], `Write ${file} -> ${got.join(",")}`);
}

// Python save is not a Write-path; Cursor only lists files after Shell actually ran.
{
  const cases: Array<{ file: string; cmd: string; out: string }> = [
    { file: "out.pptx", cmd: `python build.py && echo ${WS}/out.pptx`, out: `${WS}/out.pptx\n` },
    { file: "out.docx", cmd: `python -c 'doc.save("${WS}/out.docx")'`, out: `${WS}/out.docx\n` },
    { file: "out.xlsx", cmd: `python -c 'df.to_excel("${WS}/out.xlsx")'`, out: `${WS}/out.xlsx\n` },
    { file: "out.csv", cmd: `python -c 'df.to_csv("${WS}/out.csv")'`, out: `${WS}/out.csv\n` },
    { file: "out.html", cmd: `python -c 'df.to_html("${WS}/out.html")'`, out: `${WS}/out.html\n` },
    { file: "chart.png", cmd: `python -c 'plt.savefig("${WS}/chart.png")'`, out: `${WS}/chart.png\n` },
    { file: "out.pdf", cmd: `python -c 'c.save("${WS}/out.pdf")'`, out: `${WS}/out.pdf\n` },
    { file: "out.ods", cmd: `python -c 'wb.save("${WS}/out.ods")'`, out: `${WS}/out.ods\n` },
    { file: "out.odp", cmd: `python -c 'prs.save("${WS}/out.odp")'`, out: `${WS}/out.odp\n` },
    { file: "out.odt", cmd: `python -c 'doc.save("${WS}/out.odt")'`, out: `${WS}/out.odt\n` },
  ];
  for (const { file, cmd, out } of cases) {
    const got = turn("生成", tool("Shell", { command: cmd }, out));
    assert.ok(got.includes(file), `shell ${file} -> ${got.join(",")}`);
  }
}

// Script body is not a file list (Cursor does not scrape Write contents).
{
  assert.deepEqual(
    turn(
      "生成",
      tool("Write", { path: `${WS}/build.py`, contents: 'doc.save("out.docx")\n' }, "Wrote 20 bytes to build.py."),
    ),
    [],
  );
}

// Chat prose / backticks are not a file list.
{
  assert.deepEqual(
    turn("x", [{ type: "assistant", content: "成品在 `report.docx` 和 `deck.pptx`。" }]),
    [],
  );
}

// pptx skill: python build_pptx.py outline.json -o out.pptx
{
  assert.deepEqual(
    turn(
      "做幻灯片",
      tool(
        "Shell",
        { command: `python3 scripts/build_pptx.py outline.json -o ${WS}/pitch.pptx` },
        "wrote pitch.pptx\n",
      ),
    ),
    ["pitch.pptx"],
  );
}

// Filename with spaces, quoted.
{
  assert.deepEqual(
    turn(
      "做表",
      tool(
        "Shell",
        { command: `python build.py && ls -la "${WS}/Q3 报告.xlsx"` },
        `-rw-r-- 1 u  wheel  12 Aug 25 20:00 ${WS}/Q3 报告.xlsx\n`,
      ),
    ),
    ["Q3 报告.xlsx"],
  );
}

// LibreOffice convert stdout.
{
  const got = turn(
    "转pdf",
    tool(
      "Shell",
      { command: `soffice --headless --convert-to pdf --outdir ${WS} ${WS}/deck.pptx` },
      `convert ${WS}/deck.pptx as a PDF document -> ${WS}/deck.pdf using filter : writer_pdf_Export\n`,
    ),
  );
  assert.deepEqual(got, ["deck.pptx", "deck.pdf"]);
}

// cp dest.
{
  const got = turn("拷贝", tool("Shell", { command: `cp ${WS}/tmp.pptx ${WS}/final.pptx` }, ""));
  assert.ok(got.includes("final.pptx"), String(got));
}

// word/guide.docx is a real deliverable, not an OOXML listing.
{
  assert.deepEqual(
    turn("写", tool("Write", { path: `${WS}/word/guide.docx`, contents: "x" }, `Wrote 1 bytes to ${WS}/word/guide.docx.`)),
    ["word/guide.docx"],
  );
}

// Imagine generated image (normally junk under .grok-hyper/).
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

// Uploads stay out of 产物 (preview fallback still sees attachments).
{
  const events: SessionEvent[] = [
    { type: "user", text: "看看这个\n[attached: .grok-hyper/uploads/scan.pdf]" },
  ];
  assert.deepEqual(turnArtifacts(events, WS), []);
  assert.deepEqual(turnPreviewPaths(events, WS), [".grok-hyper/uploads/scan.pdf"]);
}

// http URLs are not workspace artifacts.
{
  assert.deepEqual(
    turn("x", [{ type: "assistant", content: "见 `https://example.com/file.pdf`" }]),
    [],
  );
}

// Directory listing without rooted paths must not harvest every existing docx.
{
  const ls = `-rw-r--r--@ 1 william  wheel  3468485 Aug 25 17:33 HLX10-002-NSCLC301-CSR-v3-TOC-fixed.docx
-rw-r--r--@ 1 william  wheel    39340 Aug 25 16:55 grok-hyper-self-intro.pptx
`;
  assert.deepEqual(turn("x", tool("Shell", { command: "ls -la" }, ls)), []);
}

// Cursor browse skip: dist / build / .next are not 产物.
{
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/dist/bundle.html`, contents: "<p/>" }, "Wrote 4 bytes to dist/bundle.html.")),
    [],
  );
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/build/out.pdf`, contents: "x" }, "Wrote 1 bytes to build/out.pdf.")),
    [],
  );
  assert.deepEqual(
    turn("x", tool("Write", { path: `${WS}/.next/static.html`, contents: "x" }, "Wrote 1 bytes to .next/static.html.")),
    [],
  );
}

// Shell command fragments are not files.
{
  const cmd = `cd /var/www/onlyoffice/documentserver/server/FileConverter/bin && ./x2t /tmp/guide.docx /tmp/guide.pdf ./font_selection.bin; echo exit:$?; ls -la /tmp/guide.pdf`;
  assert.deepEqual(turn("转pdf", tool("Shell", { command: cmd }, `${cmd}\n`)), []);
  assert.deepEqual(
    turn("x", tool("Shell", { command: `DOC="${WS}/grok-hyper-使用说明.docx"` }, "")),
    ["grok-hyper-使用说明.docx"],
  );
  assert.deepEqual(turn("x", tool("Shell", { command: "doc0=grok-hyper-使用说明.docx", }, "")), []);
}

// Markdown-only write still shows when there is no office product.
{
  assert.deepEqual(
    turn("记一下", tool("Write", { path: `${WS}/notes/out.md`, contents: "hi" }, "Wrote 2 bytes to notes/out.md.")),
    ["notes/out.md"],
  );
}

console.log("artifacts.test.ts ok");
