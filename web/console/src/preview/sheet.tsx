// DEPS: exceljs
import { useEffect, useRef, useState } from "react";
import type { PreviewProps } from "./kinds";

type Sheet = { name: string; grid: string[][] };

type ExcelCell = { value: unknown };
type ExcelRow = {
  eachCell: (opt: { includeEmpty: boolean }, cb: (cell: ExcelCell, col: number) => void) => void;
};
type ExcelWs = {
  name: string;
  rowCount: number;
  columnCount: number;
  eachRow: (opt: { includeEmpty: boolean }, cb: (row: ExcelRow, num: number) => void) => void;
  getCell: (row: number, col: number) => ExcelCell;
};
type ExcelWb = {
  worksheets: ExcelWs[];
  addWorksheet: (name?: string) => ExcelWs;
  xlsx: {
    load: (data: Uint8Array | ArrayBuffer) => Promise<unknown>;
    writeBuffer: () => Promise<ArrayBuffer | Uint8Array>;
  };
};

const MIN_ROWS = 16;
const MIN_COLS = 8;
const MAX_VIEW_ROWS = 80;
const MAX_VIEW_COLS = 20;

function isCsvPath(path: string): boolean {
  return path.toLowerCase().endsWith(".csv");
}

function colLabel(index: number): string {
  let n = index + 1;
  let s = "";
  while (n > 0) {
    s = String.fromCharCode(65 + ((n - 1) % 26)) + s;
    n = Math.floor((n - 1) / 26);
  }
  return s;
}

function parseCsv(text: string): string[][] {
  const src = text.replace(/^\uFEFF/, "");
  if (src === "") return [[""]];
  const rows: string[][] = [];
  let row: string[] = [];
  let cur = "";
  let quoted = false;
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (quoted) {
      if (ch === '"') {
        if (src[i + 1] === '"') {
          cur += '"';
          i++;
        } else {
          quoted = false;
        }
      } else {
        cur += ch;
      }
      continue;
    }
    if (ch === '"') {
      quoted = true;
      continue;
    }
    if (ch === ",") {
      row.push(cur);
      cur = "";
      continue;
    }
    if (ch === "\n") {
      row.push(cur);
      rows.push(row);
      row = [];
      cur = "";
      continue;
    }
    if (ch === "\r") continue;
    cur += ch;
  }
  row.push(cur);
  if (row.length > 1 || row[0] !== "" || rows.length === 0) rows.push(row);
  return rows;
}

function toCsv(grid: string[][]): string {
  return grid
    .map((row) =>
      row
        .map((cell) => {
          if (/[",\n\r]/.test(cell)) return `"${cell.replace(/"/g, '""')}"`;
          return cell;
        })
        .join(","),
    )
    .join("\n");
}

function padGrid(grid: string[][], minRows = MIN_ROWS, minCols = MIN_COLS): string[][] {
  let cols = minCols;
  for (const r of grid) if (r.length > cols) cols = r.length;
  const rows = Math.max(minRows, grid.length);
  const out: string[][] = [];
  for (let r = 0; r < rows; r++) {
    const row = grid[r] ? grid[r].slice() : [];
    while (row.length < cols) row.push("");
    out.push(row);
  }
  return out;
}

function trimGrid(grid: string[][]): string[][] {
  let maxR = -1;
  let maxC = -1;
  for (let r = 0; r < grid.length; r++) {
    for (let c = 0; c < grid[r].length; c++) {
      if (grid[r][c] !== "") {
        maxR = Math.max(maxR, r);
        maxC = Math.max(maxC, c);
      }
    }
  }
  if (maxR < 0) return [[""]];
  return grid.slice(0, maxR + 1).map((row) => {
    const next = row.slice(0, maxC + 1);
    while (next.length <= maxC) next.push("");
    return next;
  });
}

function cellToText(value: unknown): string {
  if (value == null || value === "") return "";
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") return String(value);
  if (value instanceof Date) return Number.isNaN(value.getTime()) ? "" : value.toLocaleString("zh-CN");
  if (typeof value !== "object") return String(value);
  const v = value as {
    formula?: string;
    result?: unknown;
    text?: string;
    richText?: Array<{ text?: string }>;
    hyperlink?: string;
    error?: unknown;
  };
  if (Array.isArray(v.richText)) return v.richText.map((t) => t.text || "").join("");
  if (typeof v.formula === "string") return `=${v.formula}`;
  if (v.result != null) return cellToText(v.result);
  if (typeof v.text === "string") return v.text;
  if (typeof v.hyperlink === "string") return v.hyperlink;
  if (v.error != null) return String(v.error);
  return "";
}

function parseInput(text: string): string | number | { formula: string } | null {
  if (text === "") return null;
  if (text.startsWith("=") && text.length > 1) return { formula: text.slice(1) };
  const t = text.trim();
  if (t !== "" && /^-?(?:0|[1-9]\d*)(?:\.\d+)?$/.test(t)) return Number(t);
  return text;
}

function sheetToGrid(ws: ExcelWs): string[][] {
  const rows = Math.max(ws.rowCount || 0, 0);
  const cols = Math.max(ws.columnCount || 0, 0);
  if (rows === 0 && cols === 0) return [];
  const grid: string[][] = Array.from({ length: rows }, () => Array.from({ length: Math.max(cols, 1) }, () => ""));
  ws.eachRow({ includeEmpty: true }, (row, r) => {
    const ri = r - 1;
    if (!grid[ri]) grid[ri] = [];
    row.eachCell({ includeEmpty: true }, (cell, c) => {
      const ci = c - 1;
      while (grid[ri].length <= ci) grid[ri].push("");
      grid[ri][ci] = cellToText(cell.value);
    });
  });
  return grid;
}

function asBytes(buf: ArrayBuffer | Uint8Array): Uint8Array {
  return buf instanceof Uint8Array ? new Uint8Array(buf) : new Uint8Array(buf);
}

async function newWorkbook(): Promise<ExcelWb> {
  // @ts-ignore exceljs is a deferred dependency (see DEPS)
  const mod = await import("exceljs");
  const rec = mod as { Workbook?: new () => ExcelWb; default?: { Workbook?: new () => ExcelWb } | (new () => ExcelWb) };
  const Ctor = rec.Workbook || (typeof rec.default === "function" ? rec.default : rec.default?.Workbook);
  if (typeof Ctor !== "function") throw new Error("exceljs 不可用");
  return new Ctor();
}

function applyCell(wb: ExcelWb | null, sheetIndex: number, r: number, c: number, text: string) {
  const ws = wb?.worksheets[sheetIndex];
  if (!ws) return;
  ws.getCell(r + 1, c + 1).value = parseInput(text);
}

async function fillWorkbook(sheets: Sheet[]): Promise<ExcelWb> {
  const wb = await newWorkbook();
  for (const s of sheets) {
    const ws = wb.addWorksheet(s.name || "Sheet1");
    const grid = trimGrid(s.grid);
    for (let r = 0; r < grid.length; r++) {
      for (let c = 0; c < grid[r].length; c++) {
        const raw = grid[r][c];
        if (raw !== "") ws.getCell(r + 1, c + 1).value = parseInput(raw);
      }
    }
  }
  if (wb.worksheets.length === 0) wb.addWorksheet("Sheet1");
  return wb;
}

async function openBook(path: string, bytes: Uint8Array): Promise<{ sheets: Sheet[]; wb: ExcelWb | null }> {
  const n = path.toLowerCase();
  if (n.endsWith(".xls") && !n.endsWith(".xlsx") && !n.endsWith(".xlsm")) {
    throw new Error("旧版 .xls 无法在浏览器里打开。请另存为 .xlsx。");
  }
  if (n.endsWith(".ods")) {
    throw new Error("OpenDocument 表格（.ods）暂不支持。请另存为 .xlsx 或 .csv。");
  }
  if (isCsvPath(path)) {
    const grid = padGrid(parseCsv(new TextDecoder().decode(bytes)));
    return { sheets: [{ name: "Sheet1", grid }], wb: null };
  }
  const wb = await newWorkbook();
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  await wb.xlsx.load(copy);
  if (wb.worksheets.length === 0) wb.addWorksheet("Sheet1");
  const sheets = wb.worksheets.map((ws) => ({ name: ws.name || "Sheet1", grid: padGrid(sheetToGrid(ws)) }));
  return { sheets, wb };
}

async function exportBook(path: string, sheets: Sheet[], wb: ExcelWb | null): Promise<Uint8Array> {
  if (isCsvPath(path)) {
    return new TextEncoder().encode(toCsv(trimGrid(sheets[0]?.grid ?? [[""]])));
  }
  const book = wb ?? (await fillWorkbook(sheets));
  return asBytes(await book.xlsx.writeBuffer());
}

export function SheetPreview({ path, bytes, url, onDirty, registerExport }: PreviewProps) {
  const [sheets, setSheets] = useState<Sheet[]>([]);
  const [active, setActive] = useState(0);
  const [err, setErr] = useState("");
  const [ready, setReady] = useState(false);
  const sheetsRef = useRef<Sheet[]>([]);
  const wbRef = useRef<ExcelWb | null>(null);

  useEffect(() => {
    let gone = false;
    setReady(false);
    setErr("");
    (async () => {
      try {
        const opened = await openBook(path, bytes);
        if (gone) return;
        sheetsRef.current = opened.sheets;
        wbRef.current = opened.wb;
        setSheets(opened.sheets);
        setActive(0);
        setReady(true);
      } catch (e) {
        if (gone) return;
        wbRef.current = null;
        sheetsRef.current = [];
        setSheets([]);
        setErr(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      gone = true;
    };
  }, [path, bytes]);

  useEffect(() => {
    registerExport(async () => exportBook(path, sheetsRef.current, wbRef.current));
  }, [path, registerExport]);

  if (err) {
    return <div className="err">无法打开表格：{err}</div>;
  }
  if (!ready) {
    return <div className="sub">加载表格…</div>;
  }

  const sheet = sheets[active] ?? sheets[0];
  if (!sheet) {
    return <div className="sub">空工作簿</div>;
  }

  const viewRows = Math.min(sheet.grid.length, MAX_VIEW_ROWS);
  const viewCols = Math.min(sheet.grid[0]?.length ?? 0, MAX_VIEW_COLS);
  const clipped = sheet.grid.length > MAX_VIEW_ROWS || (sheet.grid[0]?.length ?? 0) > MAX_VIEW_COLS;

  return (
    <div className="pv-fallback">
      {sheets.length > 1 ? (
        <div className="dt-tabs" role="tablist" aria-label="工作表">
          {sheets.map((s, i) => (
            <button
              key={`${i}:${s.name}`}
              type="button"
              role="tab"
              aria-selected={i === active}
              className={`dt-tab${i === active ? " on" : ""}`}
              onClick={() => setActive(i)}
            >
              {s.name || `工作表${i + 1}`}
            </button>
          ))}
        </div>
      ) : null}
      <p className="sub">
        {sheet.grid.length} 行 × {sheet.grid[0]?.length ?? 0} 列
        {clipped ? `（仅显示前 ${MAX_VIEW_ROWS} 行 / ${MAX_VIEW_COLS} 列，保存仍包含全部内容）` : ""}
      </p>
      <table className="pv-grid" key={active}>
        <thead>
          <tr>
            <th />
            {Array.from({ length: viewCols }, (_, c) => (
              <th key={colLabel(c)}>{colLabel(c)}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {Array.from({ length: viewRows }, (_, r) => (
            <tr key={r + 1}>
              <th>{r + 1}</th>
              {Array.from({ length: viewCols }, (_, c) => (
                <td key={colLabel(c)}>
                  <input
                    aria-label={`单元格 ${colLabel(c)}${r + 1}`}
                    defaultValue={sheet.grid[r]?.[c] ?? ""}
                    spellCheck={false}
                    autoComplete="off"
                    onChange={(e) => {
                      const v = e.target.value;
                      const cur = sheetsRef.current[active];
                      if (!cur) return;
                      if (!cur.grid[r]) cur.grid[r] = [];
                      while (cur.grid[r].length <= c) cur.grid[r].push("");
                      cur.grid[r][c] = v;
                      applyCell(wbRef.current, active, r, c, v);
                      onDirty(true);
                    }}
                  />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
