pub mod analyzer;
pub mod gantt;
pub mod parser;
pub mod report;
pub mod ui;

pub fn format_tokens(tokens: impl Into<i128>) -> String {
    let tokens = tokens.into();
    let sign = if tokens < 0 { "-" } else { "" };
    let absolute = tokens.unsigned_abs();
    if absolute >= 1_000_000 {
        format!("{sign}{:.2}M", absolute as f64 / 1_000_000.0)
    } else if absolute >= 1_000 {
        format!("{sign}{:.1}k", absolute as f64 / 1_000.0)
    } else {
        format!("{sign}{absolute}")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_token_counts_at_each_scale() {
        assert_eq!(crate::format_tokens(474), "474");
        assert_eq!(crate::format_tokens(1_000), "1.0k");
        assert_eq!(crate::format_tokens(82_500), "82.5k");
        assert_eq!(crate::format_tokens(13_580), "13.6k");
        assert_eq!(crate::format_tokens(51_780_000), "51.78M");
        assert_eq!(crate::format_tokens(364_980_000), "364.98M");
        assert_eq!(crate::format_tokens(-1_600), "-1.6k");
    }
}
