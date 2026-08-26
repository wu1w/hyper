//! Finished files the user will open. Not every turn has one.

/// Workspace folder for deliverables. Scratch and scripts stay outside.
pub const OUT_DIR: &str = "out";

/// Re-injected each real user turn (previous copy stubs). Q&A needs no file.
pub const OUT_CARD: &str = "\
[out]
If this turn creates a file the user will open (docx, pptx, xlsx, pdf, html, image), write it under out/ (create the folder). Put HTML css/js next to that html. A spoken answer needs no file.";

pub fn is_out_rel(rel: &str) -> bool {
    let n = rel.replace('\\', "/");
    let n = n.trim().trim_start_matches("./").trim_start_matches('/');
    n == OUT_DIR || n.starts_with("out/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_is_short_and_optional() {
        assert!(OUT_CARD.starts_with("[out]"));
        assert!(OUT_CARD.contains("out/"));
        assert!(OUT_CARD.contains("needs no file"));
        assert!(OUT_CARD.len() < 400);
    }

    #[test]
    fn out_rel_is_the_folder_not_out_dot_pptx() {
        assert!(is_out_rel("out/guide.docx"));
        assert!(is_out_rel("./out/index.html"));
        assert!(is_out_rel("out"));
        assert!(!is_out_rel("out.pptx"));
        assert!(!is_out_rel("notes/out.md"));
        assert!(!is_out_rel("timeout/x.docx"));
    }
}
