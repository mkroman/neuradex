//! A local Kagi Privacy Pass issuer and HTTP harness.
//!
//! The harness implements the batched-token wire format with real VOPRF keys, so tests exercise
//! blinding, proof verification, unblinding, and redemption instead of accepting arbitrary
//! tokens. Its dynamic responders redeem the `Authorization` header of each search request and
//! fail the request when a token is invalid or reused.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use httpmock::{HttpMockRequest, HttpMockResponse, Mock, MockServer};
use kagi::{Client, ClientOptions, PrivacyPassOptions};
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};
use voprf::{BlindedElement, Group, Ristretto255, VoprfServer};

use crate::kagi::{NONCE, TOKEN};

/// The Kagi Privacy Pass token type (batched Ristretto255 tokens).
const TOKEN_TYPE: u16 = 0xf91a;
/// The length of a group element.
const ELEMENT_LEN: usize = 32;
/// The length of a token nonce.
const NONCE_LEN: usize = 32;
/// The length of a challenge digest or full token key ID.
const ID_LEN: usize = 32;
/// The length of a token authenticator.
const AUTHENTICATOR_LEN: usize = 64;
/// The length of a VOPRF proof.
const PROOF_LEN: usize = 64;
/// The serialized length of a token.
const TOKEN_LEN: usize = 2 + NONCE_LEN + ID_LEN + ID_LEN + AUTHENTICATOR_LEN;

/// A local Kagi stand-in that issues and redeems real batched VOPRF tokens.
pub struct PrivacyPassHarness {
    /// The mock server backing the Kagi endpoints.
    pub server: Arc<MockServer>,
    /// The issuer shared by the issuance and stream responders.
    issuer: Arc<TestIssuer>,
}

impl PrivacyPassHarness {
    /// Starts a harness with a fresh mock server and issuer.
    #[must_use]
    pub fn start() -> Self {
        Self::with_server(Arc::new(MockServer::start()))
    }

    /// Wraps an existing mock server, so a harness and an application can share one server.
    #[must_use]
    pub fn with_server(server: Arc<MockServer>) -> Self {
        Self {
            server,
            issuer: Arc::new(TestIssuer::new()),
        }
    }

    /// Builds a client with Privacy Pass enabled and the default test batch size.
    ///
    /// # Panics
    ///
    /// Panics if the test options are invalid; they are compile-time constants.
    #[must_use]
    pub fn client(&self) -> Client {
        self.client_with_tokens(4)
    }

    /// Builds a Privacy Pass client with the given issuance batch size.
    ///
    /// # Panics
    ///
    /// Panics if the test client cannot be built.
    #[must_use]
    pub fn client_with_tokens(&self, tokens_to_stash: usize) -> Client {
        self.client_with_options(tokens_to_stash, tokens_to_stash.max(8), 0)
    }

    /// Builds a Privacy Pass client with explicit batch size, capacity, and low-water mark.
    ///
    /// # Panics
    ///
    /// Panics if the test client cannot be built.
    #[must_use]
    pub fn client_with_options(
        &self,
        tokens_to_stash: usize,
        max_tokens: usize,
        low_token_count: usize,
    ) -> Client {
        Client::with_token_and_options(
            TOKEN,
            &ClientOptions {
                base_url: self.server.base_url(),
                timeout: Duration::from_secs(5),
                privacy_pass: Some(PrivacyPassOptions {
                    tokens_to_stash,
                    max_tokens,
                    low_token_count,
                }),
                ..ClientOptions::default()
            },
        )
        .expect("the test client options are valid")
    }

    /// Mocks the old session-token document request and fails it if it is reached.
    ///
    /// Privacy Pass mode must never request `/search?token=...`; the mock turns that path into
    /// a failing response so a regression cannot silently fall back to sessions.
    #[must_use]
    pub fn mock_session_path_forbidden(&self) -> Mock<'_> {
        self.server.mock(|when, then| {
            when.method("GET")
                .path("/search")
                .query_param("token", TOKEN);
            then.status(500);
        })
    }

    /// Mocks the challenge endpoint.
    #[must_use]
    pub fn mock_challenge(&self) -> Mock<'_> {
        let header = self.issuer.challenge_header.clone();

        self.server.mock(move |when, then| {
            when.method("GET")
                .path("/pp/wwwa")
                .header("x-kagi-privacypass-client", "true")
                .header_missing("authorization")
                .header_missing("cookie");
            then.status(200)
                .header("www-authenticate", header)
                .header("content-type", "text/plain");
        })
    }

    /// Mocks the token issuance endpoint with a real VOPRF responder.
    #[must_use]
    pub fn mock_issuance(&self) -> Mock<'_> {
        let issuer = Arc::clone(&self.issuer);

        self.server.mock(move |when, then| {
            when.method("POST")
                .path("/pp/gettokens")
                .header("content-type", "application/private-token-request")
                .header("authorization", TOKEN)
                .header("x-kagi-privacypass-client", "true")
                .header_missing("cookie");
            then.respond_with(move |request: &HttpMockRequest| {
                let body = request.body().to_maybe_lossy_str();

                match issuer.issue(body.as_ref()) {
                    Ok(body) => HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "text/plain")
                        .body(body)
                        .build(),
                    Err(error) => HttpMockResponse::builder().status(500).body(error).build(),
                }
            });
        })
    }

    /// Mocks the search document and stream, redeeming a token for each request.
    #[must_use]
    pub fn mock_search_flow(&self, query: &str, fixture: &str) -> (Mock<'_>, Mock<'_>) {
        self.mock_flow("search", query, fixture, 0, 0)
    }

    /// Mocks a search flow that rejects the first `document_rejections` document requests and
    /// the first `stream_rejections` stream requests with 401.
    #[must_use]
    pub fn mock_search_flow_with_rejections(
        &self,
        query: &str,
        fixture: &str,
        document_rejections: usize,
        stream_rejections: usize,
    ) -> (Mock<'_>, Mock<'_>) {
        self.mock_flow(
            "search",
            query,
            fixture,
            document_rejections,
            stream_rejections,
        )
    }

    /// Mocks the images document and stream, redeeming a token for each request.
    #[must_use]
    pub fn mock_images_flow(&self, query: &str, fixture: &str) -> (Mock<'_>, Mock<'_>) {
        self.mock_flow("images", query, fixture, 0, 0)
    }

    /// Mocks a document and its socket stream for `endpoint`.
    fn mock_flow(
        &self,
        endpoint: &str,
        query: &str,
        fixture: &str,
        document_rejections: usize,
        stream_rejections: usize,
    ) -> (Mock<'_>, Mock<'_>) {
        let document_query = query.to_owned();
        let stream_query = query.to_owned();
        let document_path = format!("/{endpoint}");
        let stream_path = format!("/socket/{endpoint}");
        let document_responder = rejecting_response(
            document_rejections,
            authenticated_response(
                Arc::clone(&self.issuer),
                crate::kagi::nonce_body(),
                "text/html; charset=utf-8",
            ),
        );
        let stream_responder = rejecting_response(
            stream_rejections,
            authenticated_response(
                Arc::clone(&self.issuer),
                crate::kagi::read_fixture(fixture),
                "text/event-stream",
            ),
        );

        let document = self.server.mock(move |when, then| {
            when.method("GET")
                .path(document_path)
                .query_param("q", &document_query)
                .query_param_missing("token")
                .header("x-kagi-privacypass-client", "true")
                .header_matches("authorization", "^PrivateToken token=\".+\"$")
                .header_excludes("authorization", TOKEN)
                .header_missing("cookie")
                .header_missing("x-kagi-authorization")
                .header_missing("referer")
                .header_count("sec-fetch-site", ".*", 1);
            then.respond_with(document_responder);
        });

        let stream = self.server.mock(move |when, then| {
            when.method("GET")
                .path(stream_path)
                .query_param("q", &stream_query)
                .query_param("nonce", NONCE)
                .query_param_missing("token")
                .header("x-kagi-privacypass-client", "true")
                .header_matches("authorization", "^PrivateToken token=\".+\"$")
                .header_excludes("authorization", TOKEN)
                .header_missing("cookie")
                .header_missing("x-kagi-authorization")
                .header_missing("referer")
                .header("sec-fetch-site", "none")
                .header_count("sec-fetch-site", ".*", 1);
            then.respond_with(stream_responder);
        });

        (document, stream)
    }
}

/// Returns a responder that redeems the request header before serving `body`.
fn authenticated_response(
    issuer: Arc<TestIssuer>,
    body: String,
    content_type: &'static str,
) -> impl Fn(&HttpMockRequest) -> HttpMockResponse + Send + Sync + 'static {
    move |request| {
        if issuer.redeem_request(request) {
            HttpMockResponse::builder()
                .status(200)
                .header("content-type", content_type)
                .body(body.clone())
                .build()
        } else {
            HttpMockResponse::builder().status(401).build()
        }
    }
}

/// Returns a responder that rejects the first `rejections` requests with 401, then delegates.
fn rejecting_response(
    rejections: usize,
    inner: impl Fn(&HttpMockRequest) -> HttpMockResponse + Send + Sync + 'static,
) -> impl Fn(&HttpMockRequest) -> HttpMockResponse + Send + Sync + 'static {
    let remaining = AtomicUsize::new(rejections);

    move |request| {
        let rejected = remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();

        if rejected {
            HttpMockResponse::builder().status(401).build()
        } else {
            inner(request)
        }
    }
}

/// A test issuer with one VOPRF key and a double-spend guard.
struct TestIssuer {
    /// The VOPRF server key.
    server: VoprfServer<Ristretto255>,
    /// The full token key ID.
    token_key_id: [u8; ID_LEN],
    /// The raw serialized challenge whose digest is bound into each token.
    challenge: Vec<u8>,
    /// The challenge header served by `/pp/wwwa`.
    challenge_header: String,
    /// Nonces that were already redeemed.
    redeemed: Mutex<HashSet<[u8; NONCE_LEN]>>,
}

impl TestIssuer {
    /// Creates an issuer with a deterministic test key.
    ///
    /// # Panics
    ///
    /// Panics if the fixed test key cannot be derived; it is a valid VOPRF seed.
    fn new() -> Self {
        let server = VoprfServer::<Ristretto255>::new_from_seed(&[7u8; ID_LEN], b"PrivacyPass")
            .expect("the fixed test key is valid");
        let public_key = Ristretto255::serialize_elem(server.get_public_key()).to_vec();
        let token_key_id: [u8; ID_LEN] = Sha256::digest(&public_key).into();
        let challenge = challenge_bytes();
        let challenge_header = format!(
            "PrivateToken challenge={}, token-key={}",
            URL_SAFE.encode(&challenge),
            URL_SAFE.encode(&public_key)
        );

        Self {
            server,
            token_key_id,
            challenge,
            challenge_header,
            redeemed: Mutex::new(HashSet::new()),
        }
    }

    /// Issues a token response for a base64url-encoded token request.
    fn issue(&self, request: &str) -> Result<String, String> {
        let bytes = URL_SAFE
            .decode(request.trim())
            .map_err(|error| format!("invalid token request encoding: {error}"))?;
        let (key_id, elements) = parse_token_request(&bytes)?;

        if key_id != self.token_key_id[ID_LEN - 1] {
            return Err("token request used an unknown key ID".to_owned());
        }

        let mut blinded = Vec::with_capacity(elements.len());

        for element in elements {
            blinded.push(
                BlindedElement::<Ristretto255>::deserialize(&element)
                    .map_err(|error| format!("invalid blinded element: {error:?}"))?,
            );
        }

        let prepared = self
            .server
            .batch_blind_evaluate_prepare(blinded.iter())
            .collect::<Vec<_>>();
        let result = self
            .server
            .batch_blind_evaluate_finish(&mut SysRng, blinded.iter(), &prepared)
            .map_err(|error| format!("could not evaluate the token request: {error:?}"))?;
        let evaluated = result
            .messages
            .map(|message| {
                message
                    .serialize()
                    .as_slice()
                    .try_into()
                    .map_err(|_| "invalid evaluated element length".to_owned())
            })
            .collect::<Result<Vec<[u8; ELEMENT_LEN]>, String>>()?;
        let proof: [u8; PROOF_LEN] = result
            .proof
            .serialize()
            .as_slice()
            .try_into()
            .map_err(|_| "invalid proof length".to_owned())?;

        encode_token_response(&evaluated, &proof)
    }

    /// Redeems the `Authorization` header of a request.
    fn redeem_request(&self, request: &HttpMockRequest) -> bool {
        request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| self.redeem(value))
    }

    /// Returns whether the header carries a valid, unspent token.
    fn redeem(&self, authorization: &str) -> bool {
        let Some(token) = parse_authorization(authorization) else {
            return false;
        };

        if token.token_type != TOKEN_TYPE || token.token_key_id != self.token_key_id {
            return false;
        }

        let challenge_digest: [u8; ID_LEN] = Sha256::digest(&self.challenge).into();

        if token.challenge_digest != challenge_digest {
            return false;
        }

        let mut redeemed = self.redeemed.lock().unwrap_or_else(PoisonError::into_inner);

        if redeemed.contains(&token.nonce) {
            return false;
        }

        let input = token_input(token.nonce, token.challenge_digest, token.token_key_id);
        let Ok(output) = self.server.evaluate(&input) else {
            return false;
        };

        if output.to_vec() != token.authenticator.to_vec() {
            return false;
        }

        redeemed.insert(token.nonce);

        true
    }
}

/// A parsed token from an `Authorization` header.
struct WireToken {
    /// The token type.
    token_type: u16,
    /// The token nonce.
    nonce: [u8; NONCE_LEN],
    /// The challenge digest.
    challenge_digest: [u8; ID_LEN],
    /// The full token key ID.
    token_key_id: [u8; ID_LEN],
    /// The unblinded authenticator.
    authenticator: [u8; AUTHENTICATOR_LEN],
}

/// Builds the TLS-serialized challenge used by the test issuer.
fn challenge_bytes() -> Vec<u8> {
    let issuer = b"privacy-pass-issuer.kagi.com";
    let origin = b"privacy-pass-origin.kagi.com";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&TOKEN_TYPE.to_be_bytes());
    bytes.extend_from_slice(
        &u16::try_from(issuer.len())
            .expect("the issuer fits")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(issuer);
    bytes.push(0);
    bytes.extend_from_slice(
        &u16::try_from(origin.len())
            .expect("the origin fits")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(origin);

    bytes
}

/// Parses a fixed-format token request, returning its truncated key ID and elements.
fn parse_token_request(bytes: &[u8]) -> Result<(u8, Vec<[u8; ELEMENT_LEN]>), String> {
    if bytes.len() < 5 {
        return Err("token request is too short".to_owned());
    }

    let token_type = u16::from_be_bytes([bytes[0], bytes[1]]);

    if token_type != TOKEN_TYPE {
        return Err(format!("unexpected token type {token_type:#06x}"));
    }

    let key_id = bytes[2];
    let content_len = usize::from(u16::from_be_bytes([bytes[3], bytes[4]]));

    if content_len % ELEMENT_LEN != 0 || bytes.len() != 5 + content_len {
        return Err("token request has an invalid element vector".to_owned());
    }

    let (elements, remainder) = bytes[5..].as_chunks::<ELEMENT_LEN>();

    if !remainder.is_empty() {
        return Err("token request has a partial element".to_owned());
    }

    Ok((key_id, elements.to_vec()))
}

/// Serializes a fixed-format token response and returns it as base64url.
fn encode_token_response(
    evaluated: &[[u8; ELEMENT_LEN]],
    proof: &[u8; PROOF_LEN],
) -> Result<String, String> {
    let content_len = evaluated
        .len()
        .checked_mul(ELEMENT_LEN)
        .and_then(|len| u16::try_from(len).ok())
        .ok_or_else(|| "the token response is too large".to_owned())?;
    let mut bytes = Vec::with_capacity(2 + usize::from(content_len) + PROOF_LEN);
    bytes.extend_from_slice(&content_len.to_be_bytes());

    for element in evaluated {
        bytes.extend_from_slice(element);
    }

    bytes.extend_from_slice(proof);

    Ok(URL_SAFE.encode(bytes))
}

/// Parses an `Authorization: PrivateToken token="..."` header.
fn parse_authorization(value: &str) -> Option<WireToken> {
    let (scheme, rest) = value.trim().split_once(char::is_whitespace)?;

    if !scheme.eq_ignore_ascii_case("PrivateToken") {
        return None;
    }

    let (name, value) = rest.trim_start().split_once('=')?;

    if !name.trim().eq_ignore_ascii_case("token") {
        return None;
    }

    let value = value.trim().trim_matches('"');
    let bytes = URL_SAFE.decode(value).ok()?;

    if bytes.len() != TOKEN_LEN {
        return None;
    }

    let authenticator_start = TOKEN_LEN - AUTHENTICATOR_LEN;
    let token_type = u16::from_be_bytes([bytes[0], bytes[1]]);
    let nonce = bytes[2..2 + NONCE_LEN].try_into().ok()?;
    let challenge_digest = bytes[2 + NONCE_LEN..2 + NONCE_LEN + ID_LEN]
        .try_into()
        .ok()?;
    let token_key_id = bytes[2 + NONCE_LEN + ID_LEN..authenticator_start]
        .try_into()
        .ok()?;
    let authenticator = bytes[authenticator_start..].try_into().ok()?;

    Some(WireToken {
        token_type,
        nonce,
        challenge_digest,
        token_key_id,
        authenticator,
    })
}

/// Builds the VOPRF token input.
fn token_input(
    nonce: [u8; NONCE_LEN],
    challenge_digest: [u8; ID_LEN],
    token_key_id: [u8; ID_LEN],
) -> Vec<u8> {
    let mut input = Vec::with_capacity(2 + NONCE_LEN + ID_LEN + ID_LEN);
    input.extend_from_slice(&TOKEN_TYPE.to_be_bytes());
    input.extend_from_slice(&nonce);
    input.extend_from_slice(&challenge_digest);
    input.extend_from_slice(&token_key_id);

    input
}
