//! The `neuradex export-docs` subcommand.
//!
//! Renders the API documentation page — and optionally the OpenAPI document itself — to files
//! on disk. The subcommand never constructs the application state, so it requires neither
//! network access nor a `KAGI_SESSION_TOKEN`: it works from any build with the `docs` feature.

use std::fs;
use std::path::Path;

use neuradex::api::v1;

/// Renders the documentation page and writes it to `output`.
///
/// When `spec` is given, the OpenAPI document is written alongside the page as pretty-printed
/// JSON.
///
/// # Errors
///
/// Returns an error when the OpenAPI document cannot be serialized or a file cannot be written.
pub fn run(output: &Path, spec: Option<&Path>) -> Result<(), String> {
    let document = v1::openapi_spec();

    let page = neuradex_docs::render(&document);
    write(output, &page)?;

    if let Some(spec) = spec {
        let json = serde_json::to_string_pretty(&document)
            .map_err(|error| format!("could not serialize the OpenAPI document: {error}"))?;
        write(spec, &json)?;
    }

    Ok(())
}

/// Writes `contents` to `path` and reports the number of bytes written.
fn write(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;

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
}
