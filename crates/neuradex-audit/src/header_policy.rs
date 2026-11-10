//! Bounded, redacted capture of HTTP headers.
//!
//! Headers are the most dangerous part of an audit event: they routinely carry credentials,
//! session cookies, user identifiers, and IP addresses. [`HeaderPolicy`] turns an
//! [`http::HeaderMap`] into a small serializable representation with an explicit allow-list.
//! Unknown headers keep only their name, a redacted flag, and their original byte length; their
//! values never enter the event.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::digest::hmac_sha256_hex;

/// The default maximum number of captured headers per side.
pub const MAX_HEADERS: usize = 64;
/// The default maximum number of bytes captured per side, including header names.
pub const MAX_TOTAL_BYTES: usize = 8 * 1024;
/// The default maximum number of bytes captured per header value.
pub const MAX_VALUE_BYTES: usize = 512;

/// The maximum number of values captured for one header name.
const MAX_VALUES_PER_HEADER: usize = 16;

/// Headers whose values are safe to record verbatim.
const SAFE_HEADERS: &[&str] = &[
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "accept-ranges",
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
    "access-control-allow-origin",
    "access-control-expose-headers",
    "access-control-max-age",
    "age",
    "allow",
    "alt-svc",
    "cache-control",
    "connection",
    "content-encoding",
    "content-language",
    "content-length",
    "content-range",
    "content-type",
    "date",
    "etag",
    "expect",
    "expires",
    "host",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "last-modified",
    "pragma",
    "range",
    "retry-after",
    "server",
    "strict-transport-security",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "vary",
    "via",
    "warning",
    "x-content-type-options",
    "x-frame-options",
];

/// Headers whose values are credentials or personal data and are always withheld.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authenticate",
    "proxy-authorization",
    "set-cookie",
    "www-authenticate",
    "x-api-key",
    "x-auth-token",
    "x-openwebui-user-email",
    "x-openwebui-user-name",
];

/// Headers that are replaced by a deterministic HMAC-SHA256 when a key is configured.
const HASHED_HEADERS: &[&str] = &[
    "cf-connecting-ip",
    "forwarded",
    "true-client-ip",
    "x-client-ip",
    "x-cluster-client-ip",
    "x-forwarded-for",
    "x-openwebui-chat-id",
    "x-openwebui-user-id",
    "x-real-ip",
];

/// Headers whose URLs are stripped of query strings and fragments.
const SANITIZED_HEADERS: &[&str] = &["location", "referer"];

/// The policy applied to one captured header.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HeaderAction {
    /// The value is safe and recorded verbatim.
    Allow,
    /// The value is withheld entirely.
    Redact,
    /// The value is a URL whose query string and fragment are removed.
    Sanitize,
    /// The value is replaced by an HMAC-SHA256 digest.
    Hash,
}

/// The encoding of a captured header value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HeaderValueEncoding {
    /// The value is valid UTF-8.
    #[default]
    Utf8,
    /// The value was not valid UTF-8 and is base64-encoded.
    Base64,
}

/// One value of a captured header.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturedHeaderValue {
    value: String,
    encoding: HeaderValueEncoding,
    length: usize,
    truncated: bool,
}

impl CapturedHeaderValue {
    /// Returns the captured value, potentially truncated.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the encoding of the captured value.
    #[must_use]
    pub const fn encoding(&self) -> HeaderValueEncoding {
        self.encoding
    }

    /// Returns the original value length in bytes.
    #[must_use]
    pub const fn length(&self) -> usize {
        self.length
    }

    /// Whether the value was truncated by a size limit or the per-side byte budget.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// One captured header and its values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturedHeader {
    name: String,
    action: HeaderAction,
    redacted: bool,
    value_bytes: usize,
    values: Vec<CapturedHeaderValue>,
    omitted_values: usize,
}

impl CapturedHeader {
    /// Returns the lowercase header name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the policy action applied to the header.
    #[must_use]
    pub const fn action(&self) -> HeaderAction {
        self.action
    }

    /// Whether the values were withheld by the policy.
    #[must_use]
    pub const fn redacted(&self) -> bool {
        self.redacted
    }

    /// Returns the original total length of all values in bytes.
    #[must_use]
    pub const fn value_bytes(&self) -> usize {
        self.value_bytes
    }

    /// Returns the captured values.
    #[must_use]
    pub fn values(&self) -> &[CapturedHeaderValue] {
        &self.values
    }

    /// Returns the number of values omitted by the per-header cap or the byte budget.
    #[must_use]
    pub const fn omitted_values(&self) -> usize {
        self.omitted_values
    }

    /// Returns the first captured value, if any.
    #[must_use]
    pub fn first_value(&self) -> Option<&str> {
        self.values.first().map(CapturedHeaderValue::value)
    }
}

/// The bounded header capture of one request or response side.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeaderCapture {
    headers: Vec<CapturedHeader>,
    omitted_headers: usize,
    redacted_values: usize,
    truncated_values: usize,
}

impl HeaderCapture {
    /// Returns the captured headers, ordered by name.
    #[must_use]
    pub fn headers(&self) -> &[CapturedHeader] {
        &self.headers
    }

    /// Returns the number of headers omitted by the per-side header cap or byte budget.
    #[must_use]
    pub const fn omitted_headers(&self) -> usize {
        self.omitted_headers
    }

    /// Returns the number of values withheld by policy.
    #[must_use]
    pub const fn redacted_values(&self) -> usize {
        self.redacted_values
    }

    /// Returns the number of values truncated by a size limit or the byte budget.
    #[must_use]
    pub const fn truncated_values(&self) -> usize {
        self.truncated_values
    }

    /// Returns the number of captured headers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.headers.len()
    }

    /// Whether no headers were captured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Returns the captured header with `name`, ignoring case.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&CapturedHeader> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
    }
}

/// The bounds and redaction behavior used to capture headers.
#[derive(Clone)]
pub struct HeaderPolicy {
    hmac_key: Option<Vec<u8>>,
    max_headers: usize,
    max_total_bytes: usize,
    max_value_bytes: usize,
}

impl HeaderPolicy {
    /// Builds the default policy: 64 headers, 8 KiB per side, 512 bytes per value, and no HMAC
    /// key (so hashed headers are redacted).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures the HMAC-SHA256 key used to hash user identifiers and IP headers.
    ///
    /// The key must be secret and stable for the lifetime of the audit pipeline; rotating it
    /// changes every emitted digest.
    #[must_use]
    pub fn with_hmac_key(mut self, key: impl Into<Vec<u8>>) -> Self {
        self.hmac_key = Some(key.into());
        self
    }

    /// Overrides the capture bounds.
    ///
    /// A zero bound disables the corresponding output: no headers, no values, or no bytes.
    #[must_use]
    pub const fn with_limits(
        mut self,
        max_headers: usize,
        max_total_bytes: usize,
        max_value_bytes: usize,
    ) -> Self {
        self.max_headers = max_headers;
        self.max_total_bytes = max_total_bytes;
        self.max_value_bytes = max_value_bytes;
        self
    }

    /// Returns whether an HMAC key is configured.
    #[must_use]
    pub const fn has_hmac_key(&self) -> bool {
        self.hmac_key.is_some()
    }

    /// Captures `headers` under this policy.
    #[must_use]
    pub fn capture(&self, headers: &HeaderMap) -> HeaderCapture {
        let mut grouped: BTreeMap<String, Vec<&HeaderValue>> = BTreeMap::new();
        for (name, value) in headers {
            grouped
                .entry(name.as_str().to_owned())
                .or_default()
                .push(value);
        }

        let mut capture = HeaderCapture::default();
        let mut remaining = self.max_total_bytes;
        let mut captured_headers = 0_usize;

        for (name, values) in grouped {
            if captured_headers >= self.max_headers || name.len() > remaining {
                capture.omitted_headers = capture.omitted_headers.saturating_add(1);
                continue;
            }

            remaining -= name.len();
            captured_headers += 1;

            let action = classify(&name);
            let value_bytes = values
                .iter()
                .map(|value| value.as_bytes().len())
                .sum::<usize>();
            let mut header = CapturedHeader {
                name,
                action,
                redacted: false,
                value_bytes,
                values: Vec::new(),
                omitted_values: 0,
            };

            match action {
                HeaderAction::Redact => {
                    header.redacted = true;
                    capture.redacted_values = capture.redacted_values.saturating_add(values.len());
                }
                HeaderAction::Hash if self.hmac_key.is_none() => {
                    header.redacted = true;
                    capture.redacted_values = capture.redacted_values.saturating_add(values.len());
                }
                HeaderAction::Allow | HeaderAction::Sanitize | HeaderAction::Hash => {
                    let limit = values.len().min(MAX_VALUES_PER_HEADER);
                    header.omitted_values = values.len().saturating_sub(limit);

                    for value in &values[..limit] {
                        let captured = match action {
                            HeaderAction::Allow => Some(capture_plain(value)),
                            HeaderAction::Sanitize => capture_sanitized(value)
                                .map(|(text, encoding)| (text, encoding, false)),
                            HeaderAction::Hash => self.hmac_key.as_deref().map(|key| {
                                (
                                    hmac_sha256_hex(key, value.as_bytes()),
                                    HeaderValueEncoding::Utf8,
                                    false,
                                )
                            }),
                            HeaderAction::Redact => None,
                        };

                        let Some((text, encoding, mut truncated)) = captured else {
                            header.redacted = true;
                            capture.redacted_values = capture.redacted_values.saturating_add(1);
                            continue;
                        };

                        let Some((text, budget_truncated)) =
                            fit_value(text, self.max_value_bytes, &mut remaining)
                        else {
                            header.omitted_values = header.omitted_values.saturating_add(1);
                            continue;
                        };
                        truncated |= budget_truncated;

                        if truncated {
                            capture.truncated_values = capture.truncated_values.saturating_add(1);
                        }

                        header.values.push(CapturedHeaderValue {
                            value: text,
                            encoding,
                            length: value.as_bytes().len(),
                            truncated,
                        });
                    }
                }
            }

            capture.headers.push(header);
        }

        capture
    }
}

impl Default for HeaderPolicy {
    fn default() -> Self {
        Self {
            hmac_key: None,
            max_headers: MAX_HEADERS,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_value_bytes: MAX_VALUE_BYTES,
        }
    }
}

impl fmt::Debug for HeaderPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HeaderPolicy")
            .field("hmac_key_configured", &self.hmac_key.is_some())
            .field("max_headers", &self.max_headers)
            .field("max_total_bytes", &self.max_total_bytes)
            .field("max_value_bytes", &self.max_value_bytes)
            .finish()
    }
}

/// Returns the policy action for a lowercase header name.
fn classify(name: &str) -> HeaderAction {
    if SENSITIVE_HEADERS.contains(&name) {
        HeaderAction::Redact
    } else if HASHED_HEADERS.contains(&name) {
        HeaderAction::Hash
    } else if SANITIZED_HEADERS.contains(&name) {
        HeaderAction::Sanitize
    } else if SAFE_HEADERS.contains(&name) {
        HeaderAction::Allow
    } else {
        HeaderAction::Redact
    }
}

/// Captures a safe header value, base64-encoding non-UTF-8 bytes.
fn capture_plain(value: &HeaderValue) -> (String, HeaderValueEncoding, bool) {
    std::str::from_utf8(value.as_bytes()).map_or_else(
        |_| {
            (
                BASE64.encode(value.as_bytes()),
                HeaderValueEncoding::Base64,
                false,
            )
        },
        |text| (strip_controls(text), HeaderValueEncoding::Utf8, false),
    )
}

/// Captures a URL-valued header with its query string and fragment removed.
///
/// Non-UTF-8 URL values are withheld rather than encoded: a base64 rendering could still leak the
/// query string the policy exists to remove.
fn capture_sanitized(value: &HeaderValue) -> Option<(String, HeaderValueEncoding)> {
    let text = std::str::from_utf8(value.as_bytes()).ok()?;
    let sanitized = strip_query_and_fragment(&strip_controls(text));
    Some((sanitized, HeaderValueEncoding::Utf8))
}

/// Removes control characters from a header value.
fn strip_controls(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// Removes the query string and fragment from a URL-shaped value.
fn strip_query_and_fragment(value: &str) -> String {
    let end = value.find(['?', '#']).unwrap_or(value.len());

    value[..end].to_owned()
}

/// Truncates `value` to `max_value_bytes`, then to the remaining side budget, on a UTF-8 char
/// boundary. Returns the value and whether any bytes were removed.
fn fit_value(
    mut value: String,
    max_value_bytes: usize,
    remaining: &mut usize,
) -> Option<(String, bool)> {
    let mut truncated = value.len() > max_value_bytes;
    if truncated {
        truncate_bytes(&mut value, max_value_bytes);
    }

    if value.len() > *remaining {
        if *remaining == 0 {
            return None;
        }
        truncate_bytes(&mut value, *remaining);
        truncated = true;
    }

    *remaining -= value.len();
    Some((value, truncated))
}

/// Truncates `value` to at most `max_bytes` without splitting a UTF-8 scalar value.
fn truncate_bytes(value: &mut String, max_bytes: usize) {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use serde_json::Value;

    use super::*;

    fn header_map(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn safe_headers_are_captured_and_lowercased() {
        let headers = header_map(&[
            ("Content-Type", "text/html; charset=utf-8"),
            ("User-Agent", "neuradex/0.1"),
        ]);

        let capture = HeaderPolicy::new().capture(&headers);

        assert_eq!(capture.len(), 2);
        assert_eq!(capture.omitted_headers(), 0);
        assert_eq!(
            capture.get("content-type").expect("captured").first_value(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            capture.get("user-agent").expect("captured").first_value(),
            Some("neuradex/0.1")
        );
        assert_eq!(
            capture.get("content-type").expect("captured").values()[0].encoding(),
            HeaderValueEncoding::Utf8
        );
    }

    #[test]
    fn credentials_and_unknown_headers_are_redacted() {
        let headers = header_map(&[
            ("Authorization", "Bearer super-secret"),
            ("Cookie", "session=sekret"),
            ("X-Api-Key", "top-secret"),
            ("X-OpenWebUI-User-Email", "person@example.com"),
            ("X-Weird-Header", "private-value"),
        ]);

        let capture = HeaderPolicy::new().capture(&headers);
        let encoded = serde_json::to_string(&capture).expect("serializes");

        for secret in [
            "super-secret",
            "sekret",
            "top-secret",
            "person@example.com",
            "private-value",
        ] {
            assert!(!encoded.contains(secret), "{encoded}");
        }
        assert_eq!(capture.redacted_values(), 5);
        assert_eq!(capture.len(), 5);

        let weird = capture.get("x-weird-header").expect("the name is kept");
        assert!(weird.redacted());
        assert_eq!(weird.action(), HeaderAction::Redact);
        assert_eq!(weird.value_bytes(), "private-value".len());
        assert_eq!(weird.values(), []);
    }

    #[test]
    fn referer_and_location_are_sanitized() {
        let headers = header_map(&[
            ("Referer", "https://example.com/path?token=secret#fragment"),
            ("Location", "/next?continue=secret#top"),
        ]);

        let capture = HeaderPolicy::new().capture(&headers);
        let encoded = serde_json::to_string(&capture).expect("serializes");

        assert_eq!(
            capture.get("referer").expect("captured").first_value(),
            Some("https://example.com/path")
        );
        assert_eq!(
            capture.get("location").expect("captured").first_value(),
            Some("/next")
        );
        assert!(!encoded.contains("secret"), "{encoded}");
        assert!(!encoded.contains("fragment"), "{encoded}");
        assert_eq!(capture.truncated_values(), 0);
    }

    #[test]
    fn identifiers_and_ips_are_hmac_hashed_when_a_key_is_configured() {
        let headers = header_map(&[
            ("X-OpenWebUI-User-Id", "user-123"),
            ("X-OpenWebUI-Chat-Id", "chat-456"),
            ("X-Forwarded-For", "203.0.113.7, 198.51.100.9"),
        ]);
        let policy = HeaderPolicy::new().with_hmac_key(b"test-key".to_vec());

        let capture = policy.capture(&headers);
        let user_id = capture
            .get("x-openwebui-user-id")
            .expect("captured")
            .first_value()
            .expect("hashed")
            .to_owned();
        let chat_id = capture
            .get("x-openwebui-chat-id")
            .expect("captured")
            .first_value()
            .expect("hashed")
            .to_owned();
        let forwarded = capture
            .get("x-forwarded-for")
            .expect("captured")
            .first_value()
            .expect("hashed")
            .to_owned();

        assert_eq!(user_id, hmac_sha256_hex(b"test-key", b"user-123"));
        assert_eq!(chat_id, hmac_sha256_hex(b"test-key", b"chat-456"));
        assert_eq!(
            forwarded,
            hmac_sha256_hex(b"test-key", b"203.0.113.7, 198.51.100.9")
        );
        assert_eq!(user_id.len(), 64);
        assert!(!user_id.contains("user-123"));
        assert!(
            !capture
                .get("x-openwebui-user-id")
                .expect("captured")
                .redacted()
        );
    }

    #[test]
    fn hashed_headers_are_redacted_without_a_key() {
        let headers = header_map(&[("X-OpenWebUI-User-Id", "user-123")]);

        let capture = HeaderPolicy::new().capture(&headers);
        let header = capture.get("x-openwebui-user-id").expect("captured");

        assert!(header.redacted());
        assert_eq!(header.action(), HeaderAction::Hash);
        assert_eq!(header.values(), []);
        assert!(
            !serde_json::to_string(&capture)
                .expect("serializes")
                .contains("user-123")
        );
    }

    #[test]
    fn keys_change_the_digest() {
        let headers = header_map(&[("X-OpenWebUI-User-Id", "user-123")]);

        let first = HeaderPolicy::new()
            .with_hmac_key(b"first")
            .capture(&headers);
        let second = HeaderPolicy::new()
            .with_hmac_key(b"second")
            .capture(&headers);

        assert_ne!(
            first
                .get("x-openwebui-user-id")
                .expect("captured")
                .first_value(),
            second
                .get("x-openwebui-user-id")
                .expect("captured")
                .first_value()
        );
    }

    #[test]
    fn non_utf8_values_are_base64_encoded() {
        let value = HeaderValue::from_bytes(b"\xff\xfehello").expect("valid header bytes");
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", value);

        let capture = HeaderPolicy::new().capture(&headers);
        let header = capture.get("user-agent").expect("captured");
        let captured = &header.values()[0];

        assert_eq!(captured.encoding(), HeaderValueEncoding::Base64);
        assert_eq!(
            BASE64.decode(captured.value()).expect("valid base64"),
            b"\xff\xfehello"
        );
        assert_eq!(captured.length(), 7);
        assert!(!captured.truncated());
    }

    #[test]
    fn control_characters_are_stripped() {
        let value = HeaderValue::from_bytes(b"hello\tworld").expect("valid header bytes");
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", value);

        let capture = HeaderPolicy::new().capture(&headers);

        assert_eq!(
            capture.get("user-agent").expect("captured").first_value(),
            Some("helloworld")
        );
    }

    #[test]
    fn multiple_values_are_kept() {
        let mut headers = HeaderMap::new();
        headers.append("accept", HeaderValue::from_static("text/html"));
        headers.append("accept", HeaderValue::from_static("application/json"));

        let capture = HeaderPolicy::new().capture(&headers);
        let header = capture.get("accept").expect("captured");

        assert_eq!(header.values().len(), 2);
        assert_eq!(header.values()[0].value(), "text/html");
        assert_eq!(header.values()[1].value(), "application/json");
    }

    #[test]
    fn header_count_is_capped() {
        let headers = header_map(&[
            ("Accept", "text/html"),
            ("Cache-Control", "no-cache"),
            ("User-Agent", "test"),
        ]);
        let policy = HeaderPolicy::new().with_limits(2, MAX_TOTAL_BYTES, MAX_VALUE_BYTES);

        let capture = policy.capture(&headers);

        assert_eq!(capture.len(), 2);
        assert_eq!(capture.omitted_headers(), 1);
    }

    #[test]
    fn values_are_truncated_at_the_per_value_cap() {
        let long = "a".repeat(MAX_VALUE_BYTES + 100);
        let mut headers = HeaderMap::new();
        headers.insert(
            "user-agent",
            HeaderValue::from_str(&long).expect("valid header"),
        );

        let capture = HeaderPolicy::new().capture(&headers);
        let value = &capture.get("user-agent").expect("captured").values()[0];

        assert_eq!(value.value().len(), MAX_VALUE_BYTES);
        assert_eq!(value.length(), MAX_VALUE_BYTES + 100);
        assert!(value.truncated());
        assert_eq!(capture.truncated_values(), 1);
    }

    #[test]
    fn the_side_budget_bounds_names_and_values() {
        let headers = header_map(&[
            ("User-Agent", "0123456789abcdefghij"),
            ("Accept", "text/html"),
        ]);
        let policy = HeaderPolicy::new().with_limits(MAX_HEADERS, 20, MAX_VALUE_BYTES);

        let capture = policy.capture(&headers);
        let used: usize = capture
            .headers()
            .iter()
            .map(|header| {
                header.name().len()
                    + header
                        .values()
                        .iter()
                        .map(|value| value.value().len())
                        .sum::<usize>()
            })
            .sum();

        assert!(used <= 20, "captured {used} bytes");
        assert!(capture.omitted_headers() >= 1);
    }

    #[test]
    fn empty_maps_produce_empty_captures() {
        let capture = HeaderPolicy::new().capture(&HeaderMap::new());

        assert!(capture.is_empty());
        assert_eq!(capture.len(), 0);
        assert_eq!(capture.omitted_headers(), 0);
        assert_eq!(capture.redacted_values(), 0);
        assert_eq!(capture.truncated_values(), 0);
        assert_eq!(
            serde_json::to_value(&capture).expect("serializes"),
            serde_json::json!({
                "headers": [],
                "omitted_headers": 0,
                "redacted_values": 0,
                "truncated_values": 0
            })
        );
    }

    #[test]
    fn capture_shape_is_stable() {
        let headers = header_map(&[("User-Agent", "neuradex")]);
        let encoded: Value =
            serde_json::to_value(HeaderPolicy::new().capture(&headers)).expect("serializes");

        assert_eq!(encoded["headers"][0]["name"], "user-agent");
        assert_eq!(encoded["headers"][0]["action"], "allow");
        assert_eq!(encoded["headers"][0]["redacted"], false);
        assert_eq!(encoded["headers"][0]["value_bytes"], 8);
        assert_eq!(encoded["headers"][0]["values"][0]["value"], "neuradex");
        assert_eq!(encoded["headers"][0]["values"][0]["encoding"], "utf8");
        assert_eq!(encoded["headers"][0]["values"][0]["length"], 8);
        assert_eq!(encoded["headers"][0]["values"][0]["truncated"], false);
    }

    #[test]
    fn policy_debug_never_prints_the_key() {
        let policy = HeaderPolicy::new().with_hmac_key(b"super-secret-key");

        let rendered = format!("{policy:?}");

        assert!(rendered.contains("hmac_key_configured: true"));
        assert!(!rendered.contains("super-secret-key"));
    }
}
