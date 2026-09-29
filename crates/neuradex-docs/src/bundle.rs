//! Runtime CSS bundling via lightningcss.
//!
//! The theme's stylesheets are concatenated in canonical order ([`crate::assets`]) and
//! then parsed, minified, and re-serialized. Bundling is best-effort: on any error the
//! unminified concatenation is returned (with a `tracing` warning) rather than panicking
//! or failing the render — an unstylable page still documents the API.

use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions, StyleSheet};

/// Minifies `css`; returns it unchanged when lightningcss rejects it.
pub fn bundle(css: &str) -> String {
    let fallback = || {
        tracing::warn!("theme stylesheet bundling failed; serving it unminified");
        css.to_owned()
    };

    let Ok(mut sheet) = StyleSheet::parse(css, ParserOptions::default()) else {
        return fallback();
    };
    if sheet.minify(MinifyOptions::default()).is_err() {
        return fallback();
    }
    let Ok(result) = sheet.to_css(PrinterOptions {
        minify: true,
        ..PrinterOptions::default()
    }) else {
        return fallback();
    };
    result.code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets;

    #[test]
    fn minifies_simple_rules() {
        // The expected text holds `{...}` sequences, so it lives in a constant —
        // clippy's literal_string_with_formatting_args is right to be suspicious.
        const EXPECTED: &str = "a{color:red}b{color:#00f}";

        let css = bundle("a {\n  color: red;\n}\n\nb {\n  color: blue;\n}\n");
        // lightningcss minifies colors too (`blue` becomes `#00f`).
        assert_eq!(css, EXPECTED);
        assert!(!css.contains('\n'));
    }

    #[test]
    fn minifies_the_theme_without_losing_features() {
        let css = bundle(&assets::style_raw());

        assert!(!css.contains('\n'), "the bundled theme is one line");
        // The color-mix soft variants survive minification.
        assert!(css.contains("color-mix("));
        // Both theme palettes survive.
        assert!(css.contains("--page-bg"));
    }

    #[test]
    fn returns_malformed_input_unminified() {
        let malformed = "} } } { color: ;; ;
a {";
        assert_eq!(bundle(malformed), malformed);
    }
}
