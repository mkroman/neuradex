//! The neuradex documentation page, rendered from serialized OpenAPI JSON.
//!
//! This crate is presentation and content in one: it walks the serialized document — the
//! exact JSON `/openapi.json` serves — to build the page model, bundles the theme's
//! stylesheets, and renders the askama template into the single self-contained HTML
//! document the service serves at `/docs`. There is no axum here, no utoipa dependency, and
//! no derive-macro machinery beyond askama's own templates.
//!
//! # Panics
//!
//! Nothing in this crate panics on its own; see [`render`] for the one exception.

use std::borrow::Cow;
use std::sync::LazyLock;

use askama::Template;

/// Whether theme sources are re-read from disk and the docs page is re-rendered per request.
///
/// This is the single place the debug/release behavior difference is defined; the dead branch
/// is removed by the optimizer in release builds.
pub const RELOAD_SOURCES: bool = cfg!(debug_assertions);

mod assets;
mod bundle;
mod highlight;
mod html;
mod model;
mod page;
mod schema;
mod tree;

/// Renders the complete, self-contained documentation page from the serialized OpenAPI
/// document.
///
/// `spec` must be the JSON document served at `/openapi.json`; the page model preserves its
/// key order. Because the page is rendered from the same document the service serves, the
/// two cannot drift apart. Stylesheets and scripts are inlined — the output needs no
/// external assets and no runtime spec fetch.
///
/// In debug builds every call re-reads the theme sources from disk and re-bundles them,
/// so local edits show up on browser refresh without recompiling; release builds embed
/// the sources and cache the stylesheet, behavior, and bootstrap text once. The
/// document itself is walked on every call — the page-level caching (if any) is the
/// caller's, and is sound because the document is fixed per process (the router's routes
/// are static).
///
/// # Panics
///
/// Panics if the documentation-page template fails to render; the template is static and
/// its inputs are already validated, so this should not happen in practice.
#[must_use]
pub fn render(spec: &serde_json::Value) -> String {
    let page = page::build_page(spec);
    let style = page_style();
    let behavior = assets::behavior_raw();
    let theme_bootstrap = assets::bootstrap_raw();

    let template = PageTemplate {
        page: &page,
        style: &style,
        behavior: behavior.as_ref(),
        theme_bootstrap: theme_bootstrap.as_ref(),
    };
    template
        .render()
        .expect("the documentation-page template renders")
}

/// The bundled stylesheet for the page: re-bundled per call in debug builds, borrowed from a
/// process-wide cache in release builds.
fn page_style() -> Cow<'static, str> {
    if RELOAD_SOURCES {
        Cow::Owned(bundle::bundle(&assets::style_raw()))
    } else {
        static BUNDLED: LazyLock<String> = LazyLock::new(|| bundle::bundle(&assets::style_raw()));

        Cow::Borrowed(BUNDLED.as_str())
    }
}

/// The askama root template; see `templates/page.html`. The partials it includes share
/// this context (`page`, `style`, `behavior`, `theme_bootstrap`, and any loop variables
/// in scope at the include).
#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate<'a> {
    /// The page data.
    page: &'a model::Page,
    /// The bundled stylesheet, inlined into `<style>`.
    style: &'a str,
    /// The behavior scripts, inlined into the trailing `<script>`.
    behavior: &'a str,
    /// The theme bootstrap, inlined into the `<head>` script.
    theme_bootstrap: &'a str,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// A synthetic document with the same shapes the service produces: tagged operations,
    /// documented query parameters, a referenced response schema, and a self-referencing
    /// schema for the recursion guard.
    fn document() -> Value {
        json!({
            "openapi": "3.1.0",
            "info": {
                "title": "test",
                "version": "1.2.3",
                "description": "A `test` service: through `</head>` and beyond."
            },
            "tags": [
                {"name": "things", "description": "Listing things."},
                {"name": "other", "description": "Other things."}
            ],
            "paths": {
                "/ping": {"get": {
                    "tags": ["other"],
                    "summary": "Ping the service.",
                    "description": "Returns `204 No Content`.",
                    "responses": {"204": {"description": "Pong."}}
                }},
                "/things": {"get": {
                    "tags": ["things"],
                    "summary": "List things.",
                    "description": "Lists `things`.",
                    "parameters": [
                        {"name": "limit", "in": "query", "required": false,
                         "description": "Max items.",
                         "schema": {"type": "integer", "format": "int64", "minimum": 1},
                         "example": 10},
                        {"name": "kind", "in": "query", "required": false,
                         "description": "Filter by kind.",
                         "style": "form", "explode": true,
                         "schema": {"type": "array", "items": {"type": "string"}},
                         "example": ["big"]},
                        {"name": "wicked", "in": "query", "required": false,
                         "description": "Escape hatch.",
                         "schema": {"type": "string"},
                         "example": "\"></script>"}
                    ],
                    "responses": {
                        "200": {"description": "The things.",
                                "content": {"application/json": {"schema":
                                    {"$ref": "#/components/schemas/Thing"}}}},
                        "404": {"description": "No things."}
                    }
                }}
            },
            "components": {"schemas": {
                "Thing": {"type": "object", "description": "A thing.",
                          "properties": {
                              "name": {"type": "string"},
                              "next": {"$ref": "#/components/schemas/Thing"}
                          },
                          "required": ["name"]}
            }}
        })
    }

    /// A document whose operation carries a referenced JSON request body.
    fn body_document() -> Value {
        json!({
            "openapi": "3.1.0",
            "info": {"title": "test", "version": "1.2.3"},
            "tags": [{"name": "search", "description": "Searching."}],
            "paths": {
                "/search": {"post": {
                    "tags": ["search"],
                    "summary": "Search.",
                    "description": "Searches.",
                    "requestBody": {
                        "required": true,
                        "description": "The search request.",
                        "content": {"application/json": {"schema":
                            {"$ref": "#/components/schemas/SearchRequest"}}}
                    },
                    "responses": {"200": {"description": "The results."}}
                }}
            },
            "components": {"schemas": {
                "SearchRequest": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The query.",
                            "example": "rust"
                        },
                        "limit": {
                            "type": ["integer", "null"],
                            "description": "Max results.",
                            "default": 10
                        }
                    }
                }
            }}
        })
    }

    #[test]
    fn renders_the_full_page_shell() {
        let page_html = render(&document());

        assert!(page_html.starts_with("<!doctype html>"));
        assert!(page_html.contains(r#"lang="en" data-docs="neuradex""#));
        assert!(page_html.contains("<title>test · API documentation</title>"));
        assert!(page_html.contains(r#"<h1 class="wordmark">test</h1>"#));
        assert!(page_html.contains(r#"<span class="version" id="spec-version">v1.2.3</span>"#));
        assert!(page_html.contains(r#"<span id="footer-version">v1.2.3</span>"#));
        assert!(
            page_html.contains(r#"<a class="spec-link" href="/openapi.json">openapi.json</a>"#)
        );
        assert!(!page_html.contains("1 endpoint · OpenAPI 3.1.0"));
        assert!(page_html.contains("2 endpoints · OpenAPI 3.1.0"));
        // The theme assets are inlined: stylesheet, behavior scripts, and the pre-paint
        // bootstrap.
        assert!(page_html.contains("--accent-dim:"));
        assert!(page_html.contains("querySelectorAll('.tryit')"));
        assert!(page_html.contains("prefers-color-scheme"));
        assert!(page_html.contains("window.docsUI"));
    }

    #[test]
    fn renders_endpoint_sections() {
        let page_html = render(&document());

        assert!(page_html.contains(r#"<article class="ep" id="ep-things">"#));
        assert!(page_html.contains(r#"<span class="toc-num">01</span>"#));
        assert!(page_html.contains(r#"<code class="ep-path">/things</code>"#));
        assert!(page_html.contains("<code>things</code>"));
    }

    #[test]
    fn renders_parameter_tables_and_request_builders() {
        let page_html = render(&document());

        assert!(page_html.contains(r#"<span class="param-name">limit</span>"#));
        // Array parameters seed the builder input for repeated keys.
        assert!(page_html.contains(r#"data-name="kind""#));
        assert!(page_html.contains(r#"data-array="true""#));
        assert!(page_html.contains(r#"value="big""#));
        // The parameterless endpoint renders its empty-state note.
        assert!(page_html.contains("No query parameters — the endpoint takes none."));
    }

    #[test]
    fn renders_request_bodies_with_method_aware_builders() {
        let page_html = render(&body_document());

        assert!(page_html.contains(r#"data-method="POST""#));
        assert!(page_html.contains("POST /search"));
        assert!(page_html.contains(r#"aria-label="Request body""#));
        assert!(page_html.contains("application/json"));
        assert!(page_html.contains(r#"data-location="body""#));
        assert!(page_html.contains(r#"data-type="integer""#));
        assert!(page_html.contains(r#"value="rust""#));
    }

    #[test]
    fn links_responses_to_the_schema_appendix() {
        let page_html = render(&document());

        assert!(page_html.contains(r##"<a href="#schema-Thing">→ Thing</a>"##));
        assert!(page_html.contains(r#"<span class="status-pill small status-ok">200</span>"#));
        assert!(page_html.contains(r#"<span class="status-pill small status-err">404</span>"#));
        assert!(page_html.contains(r#"<div class="schema-anchor" id="schema-Thing">"#));
        assert!(page_html.contains("↺ recursive reference to Thing"));
    }

    #[test]
    fn renders_the_schema_appendix_with_tree_controls() {
        let page_html = render(&document());

        assert!(page_html.contains(r#"<article class="ep ep-appendix" id="appendix-schemas">"#));
        assert!(
            page_html.contains(r#"<div class="schema-tree"><details class="schema-node" open>"#)
        );
        assert!(page_html.contains(r#"data-tree-action="expand""#));
        assert!(page_html.contains(r#"data-tree-action="collapse""#));
    }

    #[test]
    fn escapes_markup_from_the_document() {
        let page_html = render(&document());

        assert!(page_html.contains("<code>&lt;/head&gt;</code>"));
        // The document head's own closing tag is the only raw `</head>` in the page.
        assert_eq!(page_html.match_indices("</head>").count(), 1);
    }

    #[test]
    fn escapes_seed_values_in_attributes() {
        let page_html = render(&document());

        // The seed `"></script>` cannot break out of the value attribute: askama's html
        // escaper uses numeric character references.
        assert!(page_html.contains(r#"value="&#34;&#62;&#60;/script&#62;""#));
        assert!(!page_html.contains("<script>alert"));
    }

    #[test]
    fn renders_deterministically() {
        assert_eq!(render(&document()), render(&document()));
    }
}
