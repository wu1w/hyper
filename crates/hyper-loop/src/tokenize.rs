use std::sync::OnceLock;

use tokenizers::Tokenizer;

use crate::error::{Error, Result};
use crate::family::Family;
use crate::vendor;

static QWEN38: OnceLock<Tokenizer> = OnceLock::new();

pub fn load_tokenizer(family: Family) -> Result<&'static Tokenizer> {
    match family {
        Family::Qwen38 | Family::Auto => qwen38_tokenizer(),
        Family::Grok46 => Err(Error::Tokenizer(
            "grok-4.6 uses a char estimate, not the Qwen tokenizer".into(),
        )),
        Family::Qwen35 | Family::Qwen36 => Err(Error::Tokenizer(format!(
            "{} tokenizer.json is not vendored; prefix accounting is Qwen3.8-27B only",
            family.as_str()
        ))),
    }
}

pub fn qwen38_tokenizer() -> Result<&'static Tokenizer> {
    if let Some(t) = QWEN38.get() {
        return Ok(t);
    }
    vendor::verify_qwen38()?;
    let path = vendor::tokenizer_path(Family::Qwen38);
    let tok = Tokenizer::from_file(&path).map_err(|e| Error::Tokenizer(e.to_string()))?;
    Ok(QWEN38.get_or_init(|| tok))
}

pub fn count_tokens(family: Family, text: &str) -> Result<u32> {
    if matches!(family, Family::Grok46) {
        return Ok((text.len() as u32 / 4).max(1));
    }
    let tok = load_tokenizer(family)?;
    let enc = tok
        .encode(text, false)
        .map_err(|e| Error::Tokenizer(e.to_string()))?;
    Ok(enc.get_ids().len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen38_tokenizer_loads() {
        let n = count_tokens(Family::Qwen38, "hello").unwrap();
        assert!(n > 0 && n < 10, "hello => {n} tokens");
    }

    #[test]
    fn grok46_uses_char_estimate() {
        let n = count_tokens(Family::Grok46, "hello").unwrap();
        assert_eq!(n, 1);
        let n = count_tokens(Family::Grok46, &"x".repeat(40)).unwrap();
        assert_eq!(n, 10);
    }

    #[test]
    fn cousin_tokenizer_refused() {
        let err = count_tokens(Family::Qwen35, "hello").unwrap_err();
        assert!(err.to_string().contains("not vendored"));
    }
}
