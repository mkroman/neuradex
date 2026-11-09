//! The documentation-page data model.
//!
//! These structs are the contract between the page builder ([`crate::page`]) and the askama
//! templates. Prose fields (`*_html`) carry pre-rendered HTML and are embedded verbatim;
//! every other string field is escaped by the template.

use crate::tree::SchemaNode;

/// The documentation page, as data.
///
/// All prose fields (`*_html`) carry pre-rendered HTML — built with [`crate::html::prose`]
/// or plain escaped text — and are embedded verbatim; every other string field is escaped
/// by the template.
pub struct Page {
    /// The service name; used for `<title>`, the masthead wordmark, and the footer.
    pub title: String,
    /// The service version, when known.
    pub version: Option<String>,
    /// The tagline under the masthead, as HTML.
    pub description_html: String,
    /// The URL the raw OpenAPI document is served from, e.g. `/openapi.json`.
    pub spec_url: String,
    /// The OpenAPI dialect version, e.g. `3.1.0`.
    pub spec_version: String,
    /// The number of documented endpoints.
    pub endpoint_count: usize,
    /// The table of contents groups, in display order.
    pub toc_groups: Vec<TocGroup>,
    /// The endpoint sections, in display order.
    pub endpoints: Vec<Endpoint>,
    /// The schema appendix entries, in display order.
    pub schemas: Vec<SchemaDoc>,
    /// The appendix intro line, as HTML.
    pub appendix_intro_html: String,
}

/// One tagged group of the table of contents.
pub struct TocGroup {
    /// The tag label.
    pub label: String,
    /// The tag description, shown as the label's tooltip.
    pub description: Option<String>,
    /// The group's entries, in display order.
    pub links: Vec<TocLink>,
}

/// One entry of a table-of-contents group.
pub struct TocLink {
    /// The anchor target, without the leading `#`.
    pub href: String,
    /// The zero-padded display number, e.g. `01`.
    pub num: String,
    /// The display label.
    pub label: String,
}

/// One endpoint section of the page.
pub struct Endpoint {
    /// The zero-padded display number, e.g. `01`.
    pub num: String,
    /// The section's DOM id, without the leading `#`.
    pub id: String,
    /// The HTTP method, uppercased.
    pub method: String,
    /// The endpoint path.
    pub path: String,
    /// The section heading — the operation summary, or the path when unsummarized.
    pub title: String,
    /// The operation description, as HTML.
    pub description_html: String,
    /// The query parameters, in declaration order.
    pub params: Vec<Param>,
    /// The JSON request body, when the operation documents one.
    pub body: Option<BodyView>,
    /// The documented responses, in document order.
    pub responses: Vec<ResponseView>,
    /// The success schema tree, when the operation returns JSON.
    pub schema_tree: Option<SchemaNode>,
}

/// The documented JSON request body of an endpoint.
pub struct BodyView {
    /// The media type, e.g. `application/json`.
    pub media_type: String,
    /// The request-body description, as HTML.
    pub description_html: String,
    /// The object fields, in schema declaration order.
    pub fields: Vec<Param>,
}

/// One query parameter or request-body field of an endpoint.
pub struct Param {
    /// The parameter name.
    pub name: String,
    /// Whether the parameter is required.
    pub required: bool,
    /// The parameter description, as HTML.
    pub description_html: String,
    /// The constraint annotations, e.g. `default: 5`.
    pub anns: Vec<Annotation>,
    /// The value the request builder's input is seeded with.
    pub seed: String,
    /// Whether the parameter accepts repeated/comma-separated values.
    pub is_array: bool,
    /// The schema type, e.g. `integer`, used to coerce request-builder values.
    pub schema_type: Option<String>,
}

/// One constraint annotation.
pub struct Annotation {
    /// The annotation key, e.g. `default`.
    pub key: &'static str,
    /// The annotation value, e.g. `5`.
    pub value: String,
}

impl Annotation {
    /// Constructs an annotation from a static key and a rendered value.
    pub const fn new(key: &'static str, value: String) -> Self {
        Self { key, value }
    }
}

/// One documented response of an endpoint.
pub struct ResponseView {
    /// The status code, as written in the document.
    pub code: String,
    /// Whether the status is a success/redirect (2xx/3xx).
    pub ok: bool,
    /// The response description, as HTML.
    pub description_html: String,
    /// The names of the schemas the response references.
    pub refs: Vec<String>,
}

/// One schema appendix entry.
pub struct SchemaDoc {
    /// The schema name.
    pub name: String,
    /// The anchor DOM id, without the leading `#`.
    pub anchor_id: String,
    /// The schema tree.
    pub tree: SchemaNode,
}
