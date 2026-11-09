//! Deterministic HTML pages shared by the Criterion benchmarks.
//!
//! The module is compiled into each benchmark target that declares `mod pages;`. Cargo's
//! benchmark auto-discovery is disabled in `Cargo.toml`, so this file is not a target of its
//! own.

use std::time::Duration;

use criterion::Criterion;

/// The repeated metadata markup used to grow a synthetic document head.
const META: &str = "<meta name=\"description\" content=\"Criterion benchmark metadata\">";

/// The repeated paragraph markup used to grow a synthetic document body.
const PARAGRAPH: &str = "<p>Criterion benchmark body content.</p>";

/// A generated HTML page and the byte offset where its head ends.
pub struct Page {
    /// The complete HTML document.
    pub html: String,
    /// The byte offset immediately after the closing `</head>` tag.
    pub head_len: usize,
}

impl Page {
    /// Returns the total number of bytes in the document.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.html.len()
    }
}

/// Builds a page with approximately `head_bytes` of head markup and `body_bytes` of body text.
///
/// The generated head starts with a title and is then padded with repeated metadata tags. The
/// body is padded with repeated paragraphs. Sizes are approximate: the final tag may cross the
/// requested boundary.
#[must_use]
pub fn make_page(head_bytes: usize, body_bytes: usize) -> Page {
    let mut html = String::with_capacity(head_bytes.saturating_add(body_bytes).saturating_add(64));
    html.push_str("<!doctype html><html><head><title>Criterion benchmark</title>");

    while html.len() < head_bytes {
        html.push_str(META);
    }

    html.push_str("</head>");
    let head_len = html.len();

    if body_bytes > 0 {
        html.push_str("<body>");

        while html.len().saturating_sub(head_len) < body_bytes {
            html.push_str(PARAGRAPH);
        }

        html.push_str("</body>");
    }

    html.push_str("</html>");

    Page { html, head_len }
}

/// Returns the shared Criterion configuration used by both benchmark targets.
///
/// `configure_from_args` is applied last so command-line flags such as `--sample-size` and
/// `--measurement-time` still override these defaults.
#[must_use]
pub fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .configure_from_args()
}
