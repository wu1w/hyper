//! Outline-then-chunk Read for office files. Not an OpenAI tool and not in the
//! frozen system prompt: live turns inject [`DOC_READ_CARD`] when the query or a
//! tool result names those files.

use crate::tools::doc::{is_doc_path, is_legacy_office};

/// Former always-on AGENT.md sentence. Live turns inject [`DOC_READ_CARD`].
pub const DOC_READ_SYSTEM_LINE: &str = "\
Office files: Read with no offset returns an outline; then offset is a \
1-based chunk. Grep that path. Do not Shell-cat or unzip binaries into \
context.";

/// Hidden user card: the chunk-read workflow, only when the task needs it.
pub const DOC_READ_CARD: &str = "\
[doc-read]
Office files (docx, xlsx, pptx, pdf): Read the path with no offset first — that is an outline (title → 1-based chunk), not the body. Then Grep that same path to locate, then Read offset=N limit=M where N is a chunk id, not a source line. Word: headings / ~4k-char chunks (no fake pages). PDF: pages. PPT: slides. Excel: sheets (and row ranges). Do not Shell-cat, unzip, or dump binaries. Do not scan chunks 1..N.";

pub fn wants_doc_read_card(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    if has_office_path_token(text) {
        return true;
    }
    if is_code_task(text) {
        return false;
    }
    if CHINESE_MARKS.iter().any(|m| text.contains(m)) {
        return true;
    }
    has_ascii_mark(text)
}

fn has_office_path_token(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\')))
        .any(|tok| !tok.is_empty() && (is_doc_path(tok) || is_legacy_office(tok)))
}

fn is_code_task(text: &str) -> bool {
    let lower = text.to_lowercase();
    const MARKS: &[&str] = &[
        ".py",
        ".rs",
        ".js",
        ".ts",
        ".go",
        ".java",
        "函数",
        "源码",
        "编译",
        "单测",
        "refactor",
        "compile",
        "unit test",
    ];
    MARKS.iter().any(|m| lower.contains(m))
}

fn has_ascii_mark(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|w| ASCII_MARKS.iter().any(|m| w.eq_ignore_ascii_case(m)))
}

const ASCII_MARKS: &[&str] = &[
    "docx",
    "xlsx",
    "pptx",
    "pptm",
    "ppt",
    "xlsm",
    "xls",
    "pdf",
    "powerpoint",
    "spreadsheet",
    "workbook",
    "onlyoffice",
];

const CHINESE_MARKS: &[&str] = &[
    "幻灯",
    "课件",
    "工作簿",
    "电子表格",
    "演示文稿",
    "办公文档",
    "办公文件",
    "大文档",
    "长文档",
    "word文档",
    "Word文档",
    "WORD文档",
    "招股说明书",
    "临床研究报告",
    "标书",
    "年报",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_office_paths_and_formats() {
        assert!(wants_doc_read_card(
            "打开 HLX10-002-NSCLC301-CSR-v3-TOC-fixed.docx"
        ));
        assert!(wants_doc_read_card("summarize slides.pptx"));
        assert!(wants_doc_read_card("剂量在 doses.xlsx"));
        assert!(wants_doc_read_card("把这个做成pdf"));
        assert!(wants_doc_read_card("这份大文档有多少章"));
        assert!(wants_doc_read_card("看一下演示文稿"));
        assert!(wants_doc_read_card("Glob\nreport.docx\nREADME.md"));
        assert!(wants_doc_read_card("docx"));
        assert!(wants_doc_read_card("看一下ppt"));
    }

    #[test]
    fn stays_quiet_on_ordinary_coding() {
        assert!(!wants_doc_read_card("修一下编译错误"));
        assert!(!wants_doc_read_card("refactor the loop in main.rs"));
        assert!(!wants_doc_read_card("read the note"));
        assert!(!wants_doc_read_card("use pdfplumber in extract.py"));
        assert!(!wants_doc_read_card("fix the pdf exporter in renderer.rs"));
        assert!(!wants_doc_read_card("excellent test coverage"));
        assert!(!wants_doc_read_card("帮我写个 cron"));
        assert!(!wants_doc_read_card(""));
    }

    #[test]
    fn card_names_the_call_shape() {
        assert!(DOC_READ_CARD.starts_with("[doc-read]"));
        assert!(DOC_READ_CARD.contains("no offset"));
        assert!(DOC_READ_CARD.contains("1-based chunk"));
        assert!(DOC_READ_CARD.contains("Grep"));
        assert!(DOC_READ_CARD.contains("docx"));
        assert!(DOC_READ_CARD.contains("xlsx"));
        assert!(DOC_READ_CARD.contains("pptx"));
        assert!(DOC_READ_CARD.contains("pdf"));
        assert!(!DOC_READ_SYSTEM_LINE.starts_with("[doc-read]"));
    }
}
