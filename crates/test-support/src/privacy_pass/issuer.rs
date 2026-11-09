//! The local VOPRF issuer and the Privacy Pass wire codecs used by the harness.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use httpmock::HttpMockRequest;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};
use voprf::{BlindedElement, Group, Ristretto255, VoprfServer};

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

/// A test issuer with one VOPRF key and a double-spend guard.
pub(super) struct TestIssuer {
    /// The VOPRF server key.
    server: VoprfServer<Ristretto255>,
    /// The full token key ID.
    token_key_id: [u8; ID_LEN],
    /// The raw serialized challenge whose digest is bound into each token.
    challenge: Vec<u8>,
    /// The challenge header served by `/pp/wwwa`.
    pub(super) challenge_header: String,
    /// Nonces that were already redeemed.
    redeemed: Mutex<HashSet<[u8; NONCE_LEN]>>,
}

impl TestIssuer {
    /// Creates an issuer with a deterministic test key.
    ///
    /// # Panics
    ///
    /// Panics if the fixed test key cannot be derived; it is a valid VOPRF seed.
    pub(super) fn new() -> Self {
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
    ///
    /// # Errors
    ///
    /// Returns a message describing why the request is invalid or cannot be evaluated.
    pub(super) fn issue(&self, request: &str) -> Result<String, String> {
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
    pub(super) fn redeem_request(&self, request: &HttpMockRequest) -> bool {
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

        if output.as_slice() != token.authenticator.as_slice() {
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
