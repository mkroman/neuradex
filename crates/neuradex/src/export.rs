//! The `neuradex export-docs` subcommand.
//!
//! Renders the API documentation page — and optionally the OpenAPI document itself — to files
//! on disk. The subcommand never constructs the application state, so it requires neither
//! network access nor a `KAGI_SESSION_TOKEN`: it works from any build with the `docs` feature.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use neuradex::api::v1;

/// Errors that can occur while exporting the documentation.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The OpenAPI document could not be serialized.
    #[error("could not serialize the OpenAPI document: {0}")]
    Serialize(#[from] serde_json::Error),
    /// A file could not be written.
    #[error("could not write {}: {source}", path.display())]
    Write {
        /// The path that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// Renders the documentation page and writes it to `output`.
///
/// When `spec` is given, the OpenAPI document is written alongside the page as pretty-printed
/// JSON.
///
/// # Errors
///
/// Returns an error when the OpenAPI document cannot be serialized or a file cannot be written.
pub fn run(output: &Path, spec: Option<&Path>) -> Result<(), ExportError> {
    let document = v1::openapi_spec();

    let page = neuradex_docs::render(&document);
    write(output, &page)?;

    if let Some(spec) = spec {
        let json = serde_json::to_string_pretty(&document)?;
        write(spec, &json)?;
    }

    Ok(())
}

/// Writes `contents` to `path` and reports the number of bytes written.
fn write(path: &Path, contents: &str) -> Result<(), ExportError> {
    fs::write(path, contents).map_err(|source| ExportError::Write {
        path: path.to_owned(),
        source,
    })?;

    println!("wrote {} ({} bytes)", path.display(), contents.len());

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn writes_the_documentation_page() {
        let dir = tempdir().expect("a temporary directory");
        let output = dir.path().join("docs.html");

        run(&output, None).expect("the export succeeds");

        let page = fs::read_to_string(&output).expect("the page is written");

        assert!(
            page.starts_with("<!doctype html>"),
            "unexpected page start: {:?}",
            page.get(..60).unwrap_or(&page)
        );
        assert!(
            page.contains(r#"data-docs="neuradex""#),
            "the page is missing the data-docs marker"
        );
    }

    #[test]
    fn writes_the_openapi_spec_alongside_the_page() {
        let dir = tempdir().expect("a temporary directory");
        let (output, spec) = (
            dir.path().join("docs.html"),
            dir.path().join("openapi.json"),
        );

        run(&output, Some(&spec)).expect("the export succeeds");

        let json = fs::read_to_string(&spec).expect("the spec is written");
        let document: Value = serde_json::from_str(&json).expect("valid json");
        let expected = serde_json::to_value(v1::openapi_document()).expect("the spec serializes");

        assert_eq!(document, expected);
    }

    #[test]
    fn reports_write_failures() {
        let dir = tempdir().expect("a temporary directory");
        let output = dir.path().join("missing").join("docs.html");

        let error = run(&output, None).expect_err("the export fails");

        assert!(matches!(error, ExportError::Write { .. }));
    }
}
