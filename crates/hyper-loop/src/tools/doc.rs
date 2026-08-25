//! Extract office files to text, cache by sha, outline-first then chunk Read.
//!
//! Binaries never go into the model. Read without `offset` returns a map;
//! `offset` is a 1-based chunk id, not a source line.

use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::LazyLock;

use calamine::{Data, Reader};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::Workspace;
use crate::vendor::sha256_hex;

const CACHE_VERSION: u32 = 2;
const CHUNK_CHARS: usize = 4000;
const SHEET_SPLIT_CHARS: usize = 4000;
const MAX_DOC_BYTES: u64 = 96 * 1024 * 1024;
const DOC_CHUNK_LIMIT_CAP: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedDoc {
    v: u32,
    kind: String,
    sha256: String,
    outline: Vec<OutlineEntry>,
    chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OutlineEntry {
    title: String,
    chunk: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Chunk {
    id: usize,
    title: String,
    text: String,
}

pub fn is_doc_path(path: &str) -> bool {
    matches!(
        ext_of(path).as_str(),
        "docx"
            | "docm"
            | "dotx"
            | "dotm"
            | "pptx"
            | "pptm"
            | "potx"
            | "potm"
            | "ppsx"
            | "ppsm"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "xltm"
            | "xlsb"
            | "xls"
            | "ods"
            | "pdf"
    )
}

pub fn is_legacy_office(path: &str) -> bool {
    matches!(ext_of(path).as_str(), "doc" | "dot" | "ppt" | "pot" | "pps")
}

pub fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn legacy_error(shown: &str) -> String {
    let ext = ext_of(shown);
    let suggest = match ext.as_str() {
        "doc" | "dot" => "docx",
        _ => "pptx",
    };
    format!("Error: {shown} is a legacy .{ext} file. Save as .{suggest} and Read again.")
}

pub fn read_document(
    ws: &Workspace,
    shown: &str,
    abs: &Path,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<String, String> {
    let doc = load_or_extract(ws, shown, abs)?;
    if offset.is_none() {
        return Ok(format_outline(shown, &doc));
    }
    let start = offset.unwrap_or(1).max(1) as usize;
    let requested = limit.unwrap_or(1).max(1) as usize;
    let capped = requested > DOC_CHUNK_LIMIT_CAP;
    let n = requested.min(DOC_CHUNK_LIMIT_CAP);
    let total = doc.chunks.len();
    if total == 0 {
        return Err(format!(
            "Error: {shown} extracted to 0 chunks (empty document)."
        ));
    }
    if start > total {
        return Err(format!(
            "Error: chunk {start} exceeds document length ({total} chunks)."
        ));
    }
    let end = (start + n - 1).min(total);
    let mut parts = Vec::new();
    for id in start..=end {
        let chunk = &doc.chunks[id - 1];
        parts.push(format!(
            "# {shown}  chunk {id}/{total}  {}\n---\n{}",
            chunk.title, chunk.text
        ));
    }
    let mut text = parts.join("\n\n");
    text.push_str(&format!(
        "\n[hyper sha256={}]",
        &doc.sha256[..12.min(doc.sha256.len())]
    ));
    if end < total {
        text.push_str(&format!(
            " [continue with offset={} to read the rest; {total} chunks total]",
            end + 1
        ));
    }
    if capped {
        text.push_str(&format!(
            " [limit capped at {DOC_CHUNK_LIMIT_CAP} chunks; pass offset to page instead of a huge limit]"
        ));
    }
    Ok(text)
}

pub fn grep_extracted(
    ws: &Workspace,
    shown: &str,
    abs: &Path,
    re: &Regex,
    cap: usize,
) -> Result<Vec<String>, String> {
    let doc = load_or_extract(ws, shown, abs)?;
    let mut hits = Vec::new();
    for chunk in &doc.chunks {
        for line in chunk.text.lines() {
            if re.is_match(line) {
                hits.push(format!("{shown}:chunk {}:{}", chunk.id, line));
                if hits.len() >= cap {
                    return Ok(hits);
                }
            }
        }
    }
    Ok(hits)
}

fn load_or_extract(ws: &Workspace, shown: &str, abs: &Path) -> Result<CachedDoc, String> {
    let meta = fs::metadata(abs).map_err(|e| io_err(shown, e))?;
    if meta.len() > MAX_DOC_BYTES {
        return Err(format!(
            "Error: {shown} is too large to extract (max {MAX_DOC_BYTES} bytes)."
        ));
    }
    let bytes = fs::read(abs).map_err(|e| io_err(shown, e))?;
    let sha = sha256_hex(&bytes);
    if let Some(cached) = read_cache(ws, &sha) {
        if cached.v == CACHE_VERSION && cached.sha256 == sha {
            return Ok(cached);
        }
    }
    let kind = ext_of(shown);
    let mut doc = extract_bytes(&kind, &bytes)?;
    doc.v = CACHE_VERSION;
    doc.kind = kind;
    doc.sha256 = sha.clone();
    number_chunks(&mut doc);
    write_cache(ws, &sha, &doc);
    Ok(doc)
}

fn io_err(shown: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("Error: The file {shown} does not exist.")
        }
        std::io::ErrorKind::IsADirectory => {
            format!("Error: The path {shown} is not a file.")
        }
        _ => format!("Error: Read file failed due to \n{e}"),
    }
}

fn extract_bytes(kind: &str, bytes: &[u8]) -> Result<CachedDoc, String> {
    match kind {
        "docx" | "docm" | "dotx" | "dotm" => extract_docx(bytes),
        "pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" => extract_pptx(bytes),
        "xlsx" | "xlsm" | "xltx" | "xltm" | "xlsb" | "xls" | "ods" => extract_sheet(bytes),
        "pdf" => extract_pdf(bytes),
        _ => Err(format!("Error: unsupported document kind .{kind}.")),
    }
}

fn extract_docx(bytes: &[u8]) -> Result<CachedDoc, String> {
    let xml = zip_file_text(bytes, "word/document.xml")
        .ok_or_else(|| "Error: not a valid docx (missing word/document.xml).".to_string())?;
    let heading_ids = zip_file_text(bytes, "word/styles.xml")
        .map(|s| heading_style_ids(&s))
        .unwrap_or_default();
    let paras = word_paragraphs(&xml, &heading_ids);
    let mut chunks = Vec::new();
    let mut outline = Vec::new();
    let mut buf = String::new();
    let mut title = String::from("Part 1");
    let mut pending_heading: Option<String> = None;

    let flush = |chunks: &mut Vec<Chunk>,
                 outline: &mut Vec<OutlineEntry>,
                 buf: &mut String,
                 title: &str,
                 heading: &Option<String>| {
        if buf.trim().is_empty() {
            return;
        }
        let id = chunks.len() + 1;
        if let Some(h) = heading {
            if !outline.iter().any(|e| e.chunk == id && e.title == *h) {
                outline.push(OutlineEntry {
                    title: h.clone(),
                    chunk: id,
                });
            }
        }
        chunks.push(Chunk {
            id,
            title: title.to_string(),
            text: buf.trim_end().to_string(),
        });
        buf.clear();
    };

    for para in paras {
        if para.heading {
            if !buf.trim().is_empty() {
                flush(
                    &mut chunks,
                    &mut outline,
                    &mut buf,
                    &title,
                    &pending_heading,
                );
            }
            title = if para.text.is_empty() {
                format!("Heading")
            } else {
                para.text.clone()
            };
            pending_heading = Some(title.clone());
        }
        if para.text.is_empty() {
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(&para.text);
        while buf.chars().count() > CHUNK_CHARS {
            let (head, rest) = split_prefix(&buf, CHUNK_CHARS);
            buf = head;
            flush(
                &mut chunks,
                &mut outline,
                &mut buf,
                &title,
                &pending_heading,
            );
            pending_heading = None;
            buf = rest;
            title = format!("{} (cont.)", title.trim_end_matches(" (cont.)"));
        }
    }
    flush(
        &mut chunks,
        &mut outline,
        &mut buf,
        &title,
        &pending_heading,
    );
    if chunks.is_empty() {
        chunks.push(Chunk {
            id: 1,
            title: "Part 1".into(),
            text: String::new(),
        });
    }
    if outline.is_empty() {
        outline = chunks
            .iter()
            .map(|c| OutlineEntry {
                title: c.title.clone(),
                chunk: c.id,
            })
            .collect();
    }
    Ok(CachedDoc {
        v: CACHE_VERSION,
        kind: "docx".into(),
        sha256: String::new(),
        outline,
        chunks,
    })
}

struct WordPara {
    heading: bool,
    text: String,
}

fn word_paragraphs(xml: &str, heading_ids: &HashSet<String>) -> Vec<WordPara> {
    let t_re = &*TEXT_RUN_RE;
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(rel) = find_open_tag(rest, "p") {
        let start = rel;
        let after = &rest[start..];
        let (body, consumed) = if let Some((end, clen)) = find_close(after, "p") {
            (&after[..end], end + clen)
        } else {
            break;
        };
        let inner = tag_inner(body);
        let heading = is_word_heading(inner, heading_ids);
        let text = tagged_text_runs_re(inner, t_re);
        out.push(WordPara { heading, text });
        rest = &rest[start + consumed..];
    }
    out
}

fn heading_style_ids(styles_xml: &str) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut rest = styles_xml;
    while let Some(rel) = find_open_tag(rest, "style") {
        let after = &rest[rel..];
        let (body, consumed) = if let Some((end, clen)) = find_close(after, "style") {
            (&after[..end], end + clen)
        } else {
            break;
        };
        let open = match body.find('>') {
            Some(i) => &body[..=i],
            None => body,
        };
        let ty = xml_attr(open, "type").unwrap_or_else(|| "paragraph".into());
        if ty == "character" || ty == "table" || ty == "numbering" {
            rest = &rest[rel + consumed..];
            continue;
        }
        let Some(id) = xml_attr(open, "styleId") else {
            rest = &rest[rel + consumed..];
            continue;
        };
        let name = attr_val(body, "name", "val").unwrap_or_default();
        let ol = attr_val(body, "outlineLvl", "val").and_then(|s| s.parse::<u32>().ok());
        if style_is_heading(&name, ol) {
            ids.insert(id);
        }
        rest = &rest[rel + consumed..];
    }
    ids
}

fn style_is_heading(name: &str, outline_lvl: Option<u32>) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("toc") {
        return false;
    }
    if lower.contains("char") || name.contains("字符") {
        return false;
    }
    if lower.contains("table") && lower.contains("heading") {
        return false;
    }
    if name.contains("表格标题") {
        return false;
    }
    if lower.contains("heading")
        || name.contains("标题")
        || lower == "title"
        || lower == "subtitle"
        || lower.contains("unnumbered heading")
    {
        return true;
    }
    matches!(outline_lvl, Some(n) if n <= 8)
}

fn is_word_heading(p: &str, heading_ids: &HashSet<String>) -> bool {
    if let Some(val) = pstyle_val(p) {
        if heading_ids.contains(&val) {
            return true;
        }
        let v = val.to_ascii_lowercase().replace(' ', "");
        if v.starts_with("heading") || v == "title" || v == "subtitle" {
            return true;
        }
    }
    if let Some(val) = attr_val(p, "outlineLvl", "val") {
        if val.parse::<u32>().ok().is_some_and(|n| n <= 8) {
            return true;
        }
    }
    false
}

static PSTYLE_VAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"pStyle[^>]*val\s*=\s*["']([^"']+)["']"#).expect("pStyle regex"));

static TEXT_RUN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(?:[A-Za-z0-9._-]+:)?t(?:\s[^>]*)?>(.*?)</(?:[A-Za-z0-9._-]+:)?t>")
        .expect("w:t regex")
});

fn pstyle_val(p: &str) -> Option<String> {
    PSTYLE_VAL_RE.captures(p).map(|c| unescape_xml(&c[1]))
}

fn xml_attr(hay: &str, local: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?:^|[^A-Za-z0-9]){local}\s*=\s*["']([^"']+)["']"#
    ))
    .ok()?;
    re.captures(hay).map(|c| unescape_xml(&c[1]))
}

fn extract_pptx(bytes: &[u8]) -> Result<CachedDoc, String> {
    let mut slides = zip_files_matching(bytes, |name| {
        let n = name.replace('\\', "/");
        n.starts_with("ppt/slides/slide") && n.ends_with(".xml") && !n.contains("/_rels/")
    })?;
    if slides.is_empty() {
        return Err("Error: not a valid pptx (no slides).".into());
    }
    slides.sort_by(|a, b| slide_num(&a.0).cmp(&slide_num(&b.0)));
    let mut chunks = Vec::new();
    let mut outline = Vec::new();
    for (i, (_name, xml)) in slides.iter().enumerate() {
        let text = tagged_text(&xml, "t");
        let title = first_line(&text).unwrap_or_else(|| format!("Slide {}", i + 1));
        let id = i + 1;
        outline.push(OutlineEntry {
            title: title.clone(),
            chunk: id,
        });
        chunks.push(Chunk {
            id,
            title: format!("Slide {}", i + 1),
            text,
        });
    }
    Ok(CachedDoc {
        v: CACHE_VERSION,
        kind: "pptx".into(),
        sha256: String::new(),
        outline,
        chunks,
    })
}

fn slide_num(name: &str) -> u32 {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.trim_start_matches("slide").parse().ok())
        .unwrap_or(0)
}

fn extract_sheet(bytes: &[u8]) -> Result<CachedDoc, String> {
    let cursor = Cursor::new(bytes.to_vec());
    let mut wb = calamine::open_workbook_auto_from_rs(cursor)
        .map_err(|e| format!("Error: spreadsheet extract failed ({e})."))?;
    let names = wb.sheet_names().to_vec();
    if names.is_empty() {
        return Err("Error: spreadsheet has no sheets.".into());
    }
    let mut chunks = Vec::new();
    let mut outline = Vec::new();
    for name in names {
        let range = match wb.worksheet_range(&name) {
            Ok(r) => r,
            Err(e) => {
                return Err(format!("Error: sheet `{name}` extract failed ({e})."));
            }
        };
        let mut rows: Vec<String> = Vec::new();
        for row in range.rows() {
            let line = row.iter().map(cell_text).collect::<Vec<_>>().join("\t");
            if line.chars().any(|c| !c.is_whitespace() && c != '\t') {
                rows.push(line);
            }
        }
        push_sheet_chunks(&name, &rows, &mut chunks, &mut outline);
    }
    if chunks.is_empty() {
        chunks.push(Chunk {
            id: 1,
            title: "Sheet1".into(),
            text: String::new(),
        });
        outline.push(OutlineEntry {
            title: "Sheet1".into(),
            chunk: 1,
        });
    }
    Ok(CachedDoc {
        v: CACHE_VERSION,
        kind: "xlsx".into(),
        sha256: String::new(),
        outline,
        chunks,
    })
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Float(f) => {
            if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::Error(e) => format!("#{e:?}"),
    }
}

fn push_sheet_chunks(
    sheet: &str,
    rows: &[String],
    chunks: &mut Vec<Chunk>,
    outline: &mut Vec<OutlineEntry>,
) {
    if rows.is_empty() {
        let id = chunks.len() + 1;
        outline.push(OutlineEntry {
            title: sheet.to_string(),
            chunk: id,
        });
        chunks.push(Chunk {
            id,
            title: sheet.to_string(),
            text: String::new(),
        });
        return;
    }
    let mut buf = String::new();
    let mut start_row = 1usize;
    let mut row_no = 0usize;
    let mut first = true;
    for (i, row) in rows.iter().enumerate() {
        row_no = i + 1;
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(row);
        if buf.chars().count() >= SHEET_SPLIT_CHARS && i + 1 < rows.len() {
            emit_sheet_chunk(sheet, &buf, start_row, row_no, first, chunks, outline);
            buf.clear();
            start_row = row_no + 1;
            first = false;
        }
    }
    if !buf.is_empty() {
        emit_sheet_chunk(sheet, &buf, start_row, row_no, first, chunks, outline);
    }
}

fn emit_sheet_chunk(
    sheet: &str,
    text: &str,
    start_row: usize,
    end_row: usize,
    first: bool,
    chunks: &mut Vec<Chunk>,
    outline: &mut Vec<OutlineEntry>,
) {
    let id = chunks.len() + 1;
    let title = if first && start_row == 1 {
        sheet.to_string()
    } else {
        format!("{sheet} rows {start_row}–{end_row}")
    };
    outline.push(OutlineEntry {
        title: title.clone(),
        chunk: id,
    });
    chunks.push(Chunk {
        id,
        title,
        text: text.to_string(),
    });
}

fn extract_pdf(bytes: &[u8]) -> Result<CachedDoc, String> {
    let pages = pdf_pages(bytes)?;
    if pages.iter().all(|p| p.trim().is_empty()) {
        return Err(
            "Error: could not extract text from this PDF (scanned, encrypted, or empty).".into(),
        );
    }
    let mut chunks = Vec::new();
    let mut outline = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let text = page.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let mut rest = text;
        let mut part = 0usize;
        while !rest.is_empty() {
            let (head, tail) = if rest.chars().count() > CHUNK_CHARS {
                split_prefix(&rest, CHUNK_CHARS)
            } else {
                (rest.clone(), String::new())
            };
            rest = tail;
            part += 1;
            let id = chunks.len() + 1;
            let title = if part == 1 {
                format!("Page {}", i + 1)
            } else {
                format!("Page {} (cont.)", i + 1)
            };
            if part == 1 {
                outline.push(OutlineEntry {
                    title: title.clone(),
                    chunk: id,
                });
            }
            chunks.push(Chunk {
                id,
                title,
                text: head,
            });
        }
    }
    if chunks.is_empty() {
        return Err(
            "Error: could not extract text from this PDF (scanned, encrypted, or empty).".into(),
        );
    }
    Ok(CachedDoc {
        v: CACHE_VERSION,
        kind: "pdf".into(),
        sha256: String::new(),
        outline,
        chunks,
    })
}

fn pdf_pages(bytes: &[u8]) -> Result<Vec<String>, String> {
    let by_pages = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes));
    match by_pages {
        Ok(Ok(pages)) if pages.iter().any(|p| !p.trim().is_empty()) => return Ok(pages),
        Ok(Err(e)) => {
            return fallback_pdf(bytes, Some(format!("{e}")));
        }
        Ok(Ok(_)) => return fallback_pdf(bytes, None),
        Err(_) => return fallback_pdf(bytes, Some("panic during extract".into())),
    }
}

fn fallback_pdf(bytes: &[u8], prior: Option<String>) -> Result<Vec<String>, String> {
    let one = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match one {
        Ok(Ok(text)) if text.contains('\u{c}') => {
            Ok(text.split('\u{c}').map(|s| s.to_string()).collect())
        }
        Ok(Ok(text)) if !text.trim().is_empty() => Ok(vec![text]),
        Ok(Ok(_)) => Err(pdf_fail(prior)),
        Ok(Err(e)) => Err(pdf_fail(Some(format!("{e}")))),
        Err(_) => Err(pdf_fail(prior)),
    }
}

fn pdf_fail(prior: Option<String>) -> String {
    match prior {
        Some(e) => format!("Error: PDF extract failed ({e})."),
        None => {
            "Error: could not extract text from this PDF (scanned, encrypted, or empty).".into()
        }
    }
}

fn format_outline(shown: &str, doc: &CachedDoc) -> String {
    let mut s = format!(
        "# {shown}  ({}, {} chunks, sha256={})\n\nOutline (title → chunk):\n",
        doc.kind,
        doc.chunks.len(),
        &doc.sha256[..12.min(doc.sha256.len())]
    );
    for e in &doc.outline {
        s.push_str(&format!("  {:>4}  {}\n", e.chunk, e.title));
    }
    s.push_str(
        "\nUsage: Read path offset=N limit=M  (offset is a 1-based chunk, not a source line). \
Grep this path to locate, then Read those chunks. Do not scan 1..N.\n",
    );
    s
}

fn number_chunks(doc: &mut CachedDoc) {
    for (i, c) in doc.chunks.iter_mut().enumerate() {
        c.id = i + 1;
    }
}

fn cache_dir(ws: &Workspace) -> std::path::PathBuf {
    ws.root().join(".grok-hyper").join("doc-cache")
}

fn read_cache(ws: &Workspace, sha: &str) -> Option<CachedDoc> {
    let path = cache_dir(ws).join(format!("{sha}.json"));
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(ws: &Workspace, sha: &str, doc: &CachedDoc) {
    let dir = cache_dir(ws);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{sha}.json"));
    if let Ok(raw) = serde_json::to_string(doc) {
        let _ = fs::write(path, raw);
    }
}

fn zip_file_text(bytes: &[u8], name: &str) -> Option<String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

fn zip_files_matching(
    bytes: &[u8],
    pred: impl Fn(&str) -> bool,
) -> Result<Vec<(String, String)>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("Error: not a valid office zip ({e})."))?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| format!("Error: office zip read failed ({e})."))?;
        let name = f.name().replace('\\', "/");
        if !pred(&name) {
            continue;
        }
        let mut s = String::new();
        f.read_to_string(&mut s)
            .map_err(|e| format!("Error: office zip read failed ({e})."))?;
        out.push((name, s));
    }
    Ok(out)
}

fn tagged_text(xml: &str, local: &str) -> String {
    tagged_text_blocks(xml, local)
}

fn tagged_text_blocks(xml: &str, local: &str) -> String {
    let para_tag = "p";
    let mut paras = Vec::new();
    let mut rest = xml;
    let mut found = false;
    while let Some(rel) = find_open_tag(rest, para_tag) {
        found = true;
        let after = &rest[rel..];
        let (body, consumed) = if let Some((end, clen)) = find_close(after, para_tag) {
            (&after[..end], end + clen)
        } else {
            break;
        };
        let inner = tag_inner(body);
        let t = tagged_text_runs(inner, local);
        if !t.is_empty() {
            paras.push(t);
        }
        rest = &rest[rel + consumed..];
    }
    if !found {
        return tagged_text_runs(xml, local);
    }
    paras.join("\n")
}

fn tagged_text_runs(xml: &str, local: &str) -> String {
    if local == "t" {
        return tagged_text_runs_re(xml, &TEXT_RUN_RE);
    }
    let re = Regex::new(&format!(
        r"(?s)<(?:[A-Za-z0-9._-]+:)?{local}(?:\s[^>]*)?>(.*?)</(?:[A-Za-z0-9._-]+:)?{local}>"
    ))
    .expect("tag regex");
    tagged_text_runs_re(xml, &re)
}

fn tagged_text_runs_re(xml: &str, re: &Regex) -> String {
    let mut out = String::new();
    for cap in re.captures_iter(xml) {
        out.push_str(&unescape_xml(&cap[1]));
    }
    out
}

fn find_open_tag(xml: &str, local: &str) -> Option<usize> {
    let mut i = 0;
    while i < xml.len() {
        let Some(rel) = xml[i..].find('<') else {
            return None;
        };
        let at = i + rel;
        let after_lt = &xml[at + 1..];
        if after_lt.starts_with('/') || after_lt.starts_with('!') || after_lt.starts_with('?') {
            i = at + 1;
            continue;
        }
        let rest = after_lt;
        let rest = if let Some(c) = rest.find(':') {
            if rest[..c]
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                &rest[c + 1..]
            } else {
                rest
            }
        } else {
            rest
        };
        if rest.starts_with(local) {
            let after = &rest[local.len()..];
            if after.is_empty()
                || after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
            {
                // Skip pPr / pStyle etc. that start with the same letter prefix
                // only if local is exactly the tag. `p` must not match `pPr`.
                if after.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                    i = at + 1;
                    continue;
                }
                return Some(at);
            }
        }
        i = at + 1;
    }
    None
}

fn find_close(after_open: &str, local: &str) -> Option<(usize, usize)> {
    let patterns = [
        format!("</w:{local}>"),
        format!("</a:{local}>"),
        format!("</p:{local}>"),
        format!("</{local}>"),
    ];
    let mut best: Option<(usize, usize)> = None;
    for p in &patterns {
        let hay = match best {
            Some((bi, _)) => &after_open[..bi],
            None => after_open,
        };
        if let Some(i) = hay.find(p.as_str()) {
            let cand = (i, p.len());
            best = Some(match best {
                Some(cur) if cur.0 < cand.0 || (cur.0 == cand.0 && cur.1 >= cand.1) => cur,
                _ => cand,
            });
        }
    }
    best
}

fn tag_inner(open_to_close: &str) -> &str {
    match open_to_close.find('>') {
        Some(i) => &open_to_close[i + 1..],
        None => open_to_close,
    }
}

fn attr_val<'a>(xml: &'a str, tag_local: &str, attr: &str) -> Option<String> {
    let re = Regex::new(&format!(r#"{tag_local}[^>]*{attr}\s*=\s*"([^"]+)""#)).ok()?;
    re.captures(xml)
        .or_else(|| {
            Regex::new(&format!(r#"{tag_local}[^>]*{attr}\s*=\s*'([^']+)'"#))
                .ok()
                .and_then(|r| r.captures(xml))
        })
        .map(|c| unescape_xml(&c[1]))
}

fn unescape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with("&amp;") {
            out.push('&');
            rest = &rest[5..];
        } else if rest.starts_with("&lt;") {
            out.push('<');
            rest = &rest[4..];
        } else if rest.starts_with("&gt;") {
            out.push('>');
            rest = &rest[4..];
        } else if rest.starts_with("&quot;") {
            out.push('"');
            rest = &rest[6..];
        } else if rest.starts_with("&apos;") {
            out.push('\'');
            rest = &rest[6..];
        } else if rest.starts_with("&#x") || rest.starts_with("&#X") {
            if let Some(end) = rest.find(';') {
                let hex = &rest[3..end];
                if let Ok(cp) = u32::from_str_radix(hex, 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                }
                rest = &rest[end + 1..];
            } else {
                out.push('&');
                rest = &rest[1..];
            }
        } else if rest.starts_with("&#") {
            if let Some(end) = rest.find(';') {
                let num = &rest[2..end];
                if let Ok(cp) = num.parse::<u32>() {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                }
                rest = &rest[end + 1..];
            } else {
                out.push('&');
                rest = &rest[1..];
            }
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

fn split_prefix(s: &str, n: usize) -> (String, String) {
    match s.char_indices().nth(n) {
        Some((i, _)) => (s[..i].to_string(), s[i..].to_string()),
        None => (s.to_string(), String::new()),
    }
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(80).collect())
}

#[cfg(test)]
pub(crate) fn zip_bytes(files: &[(&str, &str)]) -> Vec<u8> {
    use std::io::{Cursor, Write};
    let buf = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[cfg(test)]
pub(crate) fn fixture_docx() -> Vec<u8> {
    let document = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:pPr><w:pStyle w:val="Heading1"/></w:pPr>
      <w:r><w:t>Introduction</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>UNIQUE_BODY_SENTENCE_DOSAGE and 剂量 notes.</w:t></w:r>
    </w:p>
    <w:p>
      <w:pPr><w:pStyle w:val="Heading1"/></w:pPr>
      <w:r><w:t>Methods</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>Measure twice.</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#;
    zip_bytes(&[("word/document.xml", document)])
}

#[cfg(test)]
pub(crate) fn fixture_pptx() -> Vec<u8> {
    let slide = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree><p:sp><p:txBody>
    <a:p><a:r><a:t>Hello slide</a:t></a:r></a:p>
    <a:p><a:r><a:t>UNIQUE_BODY_SENTENCE_DOSAGE</a:t></a:r></a:p>
  </p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
    zip_bytes(&[("ppt/slides/slide1.xml", slide)])
}

#[cfg(test)]
pub(crate) fn fixture_xlsx() -> Vec<u8> {
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
    let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let wb_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Doses" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
    let sheet = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>UNIQUE_BODY_SENTENCE_DOSAGE</t></is></c></row>
    <row r="2"><c r="A2" t="inlineStr"><is><t>42</t></is></c></row>
  </sheetData>
</worksheet>"#;
    zip_bytes(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", rels),
        ("xl/_rels/workbook.xml.rels", wb_rels),
        ("xl/workbook.xml", workbook),
        ("xl/worksheets/sheet1.xml", sheet),
    ])
}

#[cfg(test)]
pub(crate) fn fixture_pdf() -> Vec<u8> {
    build_pdf(&["Page one hello", "Page two UNIQUE_BODY_SENTENCE_DOSAGE"])
}

#[cfg(test)]
fn build_pdf(page_texts: &[&str]) -> Vec<u8> {
    let n_pages = page_texts.len();
    let page_ids: Vec<usize> = (0..n_pages).map(|i| 4 + i).collect();
    let content_ids: Vec<usize> = (0..n_pages).map(|i| 4 + n_pages + i).collect();
    let kids = page_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut map: Vec<(usize, String)> = Vec::new();
    map.push((1, "<< /Type /Catalog /Pages 2 0 R >>".into()));
    map.push((
        2,
        format!("<< /Type /Pages /Count {n_pages} /Kids [{kids}] >>"),
    ));
    map.push((
        3,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ));
    for (i, text) in page_texts.iter().enumerate() {
        let pid = page_ids[i];
        let cid = content_ids[i];
        map.push((
            pid,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {cid} 0 R /Resources << /Font << /F1 3 0 R >> >> >>"
            ),
        ));
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let stream = format!("BT /F1 12 Tf 72 720 Td ({escaped}) Tj ET");
        map.push((
            cid,
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
        ));
    }
    map.sort_by_key(|(id, _)| *id);
    let mut out = b"%PDF-1.4\n".to_vec();
    let max_id = map.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &map {
        offsets[*id] = out.len();
        out.extend(format!("{id} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    let xref_at = out.len();
    out.extend(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend(b"0000000000 65535 f \n");
    for id in 1..=max_id {
        out.extend(format!("{:010} 00000 n \n", offsets[id]).as_bytes());
    }
    out.extend(
        format!(
            "trailer << /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_at
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docx_numeric_style_ids_resolve_via_styles_xml() {
        let styles = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:style w:type="paragraph" w:styleId="1">
    <w:name w:val="heading 1"/>
    <w:pPr><w:outlineLvl w:val="0"/></w:pPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="20">
    <w:name w:val="heading 2"/>
    <w:pPr><w:outlineLvl w:val="1"/></w:pPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="TOC1">
    <w:name w:val="toc 1"/>
  </w:style>
</w:styles>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="1"/></w:pPr><w:r><w:t>SYNOPSIS</w:t></w:r></w:p>
    <w:p><w:r><w:t>UNIQUE_BODY_SENTENCE_DOSAGE</w:t></w:r></w:p>
    <w:p><w:pPr><w:pStyle w:val="20"/></w:pPr><w:r><w:t>Trial Objectives</w:t></w:r></w:p>
    <w:p><w:pPr><w:pStyle w:val="TOC1"/></w:pPr><w:r><w:t>Should not be an outline heading</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        let bytes = zip_bytes(&[("word/document.xml", document), ("word/styles.xml", styles)]);
        let doc = extract_docx(&bytes).unwrap();
        let titles: Vec<_> = doc.outline.iter().map(|e| e.title.as_str()).collect();
        assert!(titles.contains(&"SYNOPSIS"), "{titles:?}");
        assert!(titles.contains(&"Trial Objectives"), "{titles:?}");
        assert!(
            !titles.iter().any(|t| t.contains("Should not")),
            "{titles:?}"
        );
    }

    #[test]
    fn docx_outline_has_headings_not_body() {
        let doc = extract_docx(&fixture_docx()).unwrap();
        let titles: Vec<_> = doc.outline.iter().map(|e| e.title.as_str()).collect();
        assert!(titles.contains(&"Introduction"), "{titles:?}");
        assert!(titles.contains(&"Methods"), "{titles:?}");
        let joined_outline: String = doc.outline.iter().map(|e| e.title.clone()).collect();
        assert!(
            !joined_outline.contains("UNIQUE_BODY_SENTENCE_DOSAGE"),
            "{joined_outline}"
        );
        assert!(doc.chunks[0].text.contains("UNIQUE_BODY_SENTENCE_DOSAGE"));
        assert!(doc.chunks.iter().any(|c| c.text.contains("剂量")));
    }

    #[test]
    fn pptx_one_slide_chunk() {
        let doc = extract_pptx(&fixture_pptx()).unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].text.contains("Hello slide"));
        assert!(doc.chunks[0].text.contains("UNIQUE_BODY_SENTENCE_DOSAGE"));
    }

    #[test]
    fn xlsx_sheet_named_in_outline() {
        let doc = extract_sheet(&fixture_xlsx()).unwrap();
        assert!(
            doc.outline.iter().any(|e| e.title == "Doses"),
            "{:?}",
            doc.outline.iter().map(|e| &e.title).collect::<Vec<_>>()
        );
        assert!(doc.chunks[0].text.contains("UNIQUE_BODY_SENTENCE_DOSAGE"));
    }

    #[test]
    fn pdf_two_pages() {
        let doc = extract_pdf(&fixture_pdf()).unwrap();
        assert!(
            doc.chunks.len() >= 2,
            "expected per-page chunks, got {:?}",
            doc.chunks.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
        let all: String = doc.chunks.iter().map(|c| c.text.clone()).collect();
        assert!(
            all.contains("Page one hello") || all.contains("hello"),
            "{all}"
        );
        assert!(all.contains("UNIQUE_BODY_SENTENCE_DOSAGE"), "{all}");
    }

    #[test]
    fn legacy_ext_is_not_doc_path() {
        assert!(is_legacy_office("old.doc"));
        assert!(!is_doc_path("old.doc"));
        assert!(is_doc_path("report.DOCX"));
        assert!(is_doc_path("a.pdf"));
    }
}
