//! The `neuradex export-docs` subcommand.
//!
//! Renders the API documentation page — and optionally the OpenAPI document itself — to files
//! on disk. The subcommand never constructs the application state, so it requires neither
//! network access nor a `KAGI_SESSION_TOKEN`: it works from any build with the `docs` feature.

use std::fs;
use std::path::Path;

use neuradex::api::v1::openapi_document;

/// Renders the documentation page and writes it to `output`.
///
/// When `spec` is given, the OpenAPI document is written alongside the page as pretty-printed
/// JSON.
///
/// # Errors
///
/// Returns an error when the OpenAPI document cannot be serialized or a file cannot be written.
pub fn run(output: &Path, spec: Option<&Path>) -> Result<(), String> {
    let document = openapi_document();

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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::Value;

    use super::*;

    /// Returns a unique scratch directory for a test, creating it.
    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the unix epoch")
            .as_nanos();

        let dir = std::env::temp_dir().join(format!(
            "neuradex-export-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("the scratch directory is created");

        dir
    }

    #[test]
    fn writes_the_documentation_page() {
        let output = scratch_dir("page").join("docs.html");

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
        let dir = scratch_dir("spec");
        let (output, spec) = (dir.join("docs.html"), dir.join("openapi.json"));

        run(&output, Some(&spec)).expect("the export succeeds");

        let json = fs::read_to_string(&spec).expect("the spec is written");
        let document: Value = serde_json::from_str(&json).expect("valid json");

        for path in ["/healthz", "/v1/fetch", "/v1/peek", "/v1/search"] {
            assert!(document["paths"].get(path).is_some(), "missing {path}");
        }
    }
}
