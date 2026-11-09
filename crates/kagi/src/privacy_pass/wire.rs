//! Fixed-size Privacy Pass wire formats and VOPRF token operations.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use rand::TryRng;
use rand::rngs::SysRng;
use reqwest::header::HeaderValue;
use sha2::{Digest, Sha256};
use voprf::{EvaluationElement, Group, Proof, Ristretto255, VoprfClient};

use super::token::PrivacyToken;
use super::{
    AUTHENTICATOR_LEN, CHALLENGE_DIGEST_LEN, ELEMENT_LEN, NONCE_LEN, PROOF_LEN, PrivacyPassError,
    TOKEN_KEY_ID_LEN, TOKEN_TYPE,
};

/// A batched-token client alias.
type TokenClient = VoprfClient<Ristretto255>;
/// A VOPRF public key.
type PublicKey = <Ristretto255 as Group>::Elem;

/// The parsed issuer challenge.
pub(super) struct Challenge {
    /// The raw TLS-serialized `TokenChallenge`, hashed for the token input.
    serialized: Vec<u8>,
    /// The issuer's VOPRF public key.
    pub(super) public_key: PublicKey,
    /// The full token key ID derived from the public key.
    token_key_id: [u8; TOKEN_KEY_ID_LEN],
}

/// A token that has been blinded and is waiting for the issuer's response.
pub(super) struct PendingToken {
    /// The VOPRF client state needed to unblind the response.
    client: TokenClient,
    /// The serialized token input.
    input: Vec<u8>,
    /// The token nonce.
    nonce: [u8; NONCE_LEN],
    /// The challenge digest.
    challenge_digest: [u8; CHALLENGE_DIGEST_LEN],
    /// The full token key ID.
    token_key_id: [u8; TOKEN_KEY_ID_LEN],
}

/// A batched token request in the fixed Kagi wire format.
pub(super) struct TokenRequest {
    /// The truncated token key ID.
    truncated_token_key_id: u8,
    /// The blinded group elements.
    blinded_elements: Vec<[u8; ELEMENT_LEN]>,
}

/// A parsed batched token response.
pub(super) struct TokenResponse {
    /// The evaluated group elements.
    evaluated_elements: Vec<[u8; ELEMENT_LEN]>,
    /// The VOPRF proof.
    proof: [u8; PROOF_LEN],
}

/// Parses a `WWW-Authenticate` challenge header.
///
/// Kagi sends one `PrivateToken` challenge with `challenge` and `token-key` parameters, and may
/// also include `max-age`. Unknown parameters and multiple challenges are rejected, matching
/// the reference implementation.
pub(super) fn parse_challenge(value: &HeaderValue) -> Result<Challenge, PrivacyPassError> {
    let text = value
        .to_str()
        .map_err(|_| PrivacyPassError::InvalidChallenge)?;
    let mut challenges = 0usize;
    let mut serialized = None;
    let mut token_key = None;

    for part in text.split(',') {
        let part = part.trim();
        let (part, is_scheme) = match part.get(.."PrivateToken".len()) {
            Some(prefix) if prefix.eq_ignore_ascii_case("PrivateToken") => {
                challenges = challenges.saturating_add(1);

                if challenges > 1 {
                    return Err(PrivacyPassError::ChallengeCount(challenges));
                }

                (part["PrivateToken".len()..].trim_start(), true)
            }
            _ => (part, false),
        };

        if part.is_empty() {
            continue;
        }

        let Some((name, raw_value)) = part.split_once('=') else {
            return Err(PrivacyPassError::InvalidChallenge);
        };
        let name = name.trim().to_ascii_lowercase();
        let raw_value = raw_value.trim().trim_matches('"');

        match name.as_str() {
            "challenge" => {
                if serialized.is_some() {
                    return Err(PrivacyPassError::InvalidChallenge);
                }

                serialized = Some(
                    URL_SAFE
                        .decode(raw_value)
                        .map_err(|_| PrivacyPassError::InvalidChallenge)?,
                );
            }
            "token-key" => {
                if token_key.is_some() {
                    return Err(PrivacyPassError::InvalidChallenge);
                }

                token_key = Some(
                    URL_SAFE
                        .decode(raw_value)
                        .map_err(|_| PrivacyPassError::InvalidChallenge)?,
                );
            }
            "max-age" => {
                if is_scheme {
                    return Err(PrivacyPassError::InvalidChallenge);
                }
            }
            _ => return Err(PrivacyPassError::InvalidChallenge),
        }
    }

    if challenges != 1 {
        return Err(PrivacyPassError::ChallengeCount(challenges));
    }

    let serialized = serialized.ok_or(PrivacyPassError::InvalidChallenge)?;
    let token_key = token_key.ok_or(PrivacyPassError::InvalidChallenge)?;

    if serialized.len() < 2 || u16::from_be_bytes([serialized[0], serialized[1]]) != TOKEN_TYPE {
        return Err(PrivacyPassError::InvalidChallenge);
    }

    let public_key = Ristretto255::deserialize_elem(&token_key)
        .map_err(|_| PrivacyPassError::InvalidChallenge)?;
    let serialized_key = Ristretto255::serialize_elem(public_key);
    let token_key_id: [u8; TOKEN_KEY_ID_LEN] = Sha256::digest(serialized_key.as_slice()).into();

    Ok(Challenge {
        serialized,
        public_key,
        token_key_id,
    })
}

/// Blinds a batch of tokens and returns the request that should be sent to the issuer.
pub(super) fn prepare_request(
    challenge: &Challenge,
    count: u16,
) -> Result<(TokenRequest, Vec<PendingToken>), PrivacyPassError> {
    let challenge_digest: [u8; CHALLENGE_DIGEST_LEN] = Sha256::digest(&challenge.serialized).into();
    let mut rng = SysRng;
    let mut blinded_elements = Vec::with_capacity(usize::from(count));
    let mut pending = Vec::with_capacity(usize::from(count));

    for _ in 0..count {
        let mut nonce = [0u8; NONCE_LEN];
        rng.try_fill_bytes(&mut nonce)
            .map_err(|_| PrivacyPassError::Random)?;

        let input = token_input(nonce, challenge_digest, challenge.token_key_id);
        let blinded =
            TokenClient::blind(&input, &mut rng).map_err(|_| PrivacyPassError::Blinding)?;
        let element: [u8; ELEMENT_LEN] = blinded
            .message
            .serialize()
            .as_slice()
            .try_into()
            .map_err(|_| PrivacyPassError::Serialize)?;

        blinded_elements.push(element);
        pending.push(PendingToken {
            client: blinded.state,
            input,
            nonce,
            challenge_digest,
            token_key_id: challenge.token_key_id,
        });
    }

    let request = TokenRequest {
        truncated_token_key_id: challenge.token_key_id[TOKEN_KEY_ID_LEN - 1],
        blinded_elements,
    };

    Ok((request, pending))
}

/// Builds the token input from its fixed-size components.
fn token_input(
    nonce: [u8; NONCE_LEN],
    challenge_digest: [u8; CHALLENGE_DIGEST_LEN],
    token_key_id: [u8; TOKEN_KEY_ID_LEN],
) -> Vec<u8> {
    let mut input = Vec::with_capacity(2 + NONCE_LEN + CHALLENGE_DIGEST_LEN + TOKEN_KEY_ID_LEN);
    input.extend_from_slice(&TOKEN_TYPE.to_be_bytes());
    input.extend_from_slice(&nonce);
    input.extend_from_slice(&challenge_digest);
    input.extend_from_slice(&token_key_id);

    input
}

/// Serializes a token request in the fixed Kagi wire format.
pub(super) fn serialize_token_request(request: &TokenRequest) -> Result<Vec<u8>, PrivacyPassError> {
    let content_len = request
        .blinded_elements
        .len()
        .checked_mul(ELEMENT_LEN)
        .and_then(|len| u16::try_from(len).ok())
        .ok_or(PrivacyPassError::Serialize)?;
    let mut bytes = Vec::with_capacity(5 + usize::from(content_len));
    bytes.extend_from_slice(&TOKEN_TYPE.to_be_bytes());
    bytes.push(request.truncated_token_key_id);
    bytes.extend_from_slice(&content_len.to_be_bytes());

    for element in &request.blinded_elements {
        bytes.extend_from_slice(element);
    }

    Ok(bytes)
}

/// Parses a token response from the fixed Kagi wire format.
pub(super) fn parse_token_response(bytes: &[u8]) -> Result<TokenResponse, PrivacyPassError> {
    let length_bytes: [u8; 2] = bytes
        .get(..2)
        .ok_or(PrivacyPassError::InvalidTokenResponse)?
        .try_into()
        .map_err(|_| PrivacyPassError::InvalidTokenResponse)?;
    let content_len = usize::from(u16::from_be_bytes(length_bytes));

    if content_len % ELEMENT_LEN != 0 {
        return Err(PrivacyPassError::InvalidTokenResponse);
    }

    let expected_len = 2 + content_len + PROOF_LEN;

    if bytes.len() != expected_len {
        return Err(PrivacyPassError::InvalidTokenResponse);
    }

    let mut evaluated_elements = Vec::with_capacity(content_len / ELEMENT_LEN);

    for chunk in bytes[2..2 + content_len].as_chunks::<ELEMENT_LEN>().0 {
        evaluated_elements.push(*chunk);
    }

    let proof = bytes[2 + content_len..]
        .try_into()
        .map_err(|_| PrivacyPassError::InvalidTokenResponse)?;

    Ok(TokenResponse {
        evaluated_elements,
        proof,
    })
}

/// Verifies the VOPRF proof and unblinds the evaluated elements into tokens.
pub(super) fn finalize_tokens(
    response: &TokenResponse,
    pending: Vec<PendingToken>,
    public_key: PublicKey,
) -> Result<Vec<PrivacyToken>, PrivacyPassError> {
    let count = pending.len().min(response.evaluated_elements.len());

    if count == 0 {
        return Err(PrivacyPassError::EmptyBatch);
    }

    let mut pending = pending;
    pending.truncate(count);

    let evaluated = response
        .evaluated_elements
        .iter()
        .take(count)
        .map(|element| {
            EvaluationElement::<Ristretto255>::deserialize(element)
                .map_err(|_| PrivacyPassError::InvalidTokenResponse)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let proof = Proof::<Ristretto255>::deserialize(&response.proof)
        .map_err(|_| PrivacyPassError::InvalidTokenResponse)?;
    let inputs = pending
        .iter()
        .map(|token| token.input.clone())
        .collect::<Vec<_>>();
    let clients = pending
        .iter()
        .map(|token| token.client.clone())
        .collect::<Vec<_>>();
    let results = TokenClient::batch_finalize(&inputs, &clients, &evaluated, &proof, public_key)
        .map_err(|_| PrivacyPassError::InvalidTokenResponse)?;
    let mut tokens = Vec::with_capacity(count);

    for (result, pending) in results.zip(pending) {
        let authenticator = result.map_err(|_| PrivacyPassError::InvalidTokenResponse)?;
        let authenticator: [u8; AUTHENTICATOR_LEN] = authenticator
            .as_slice()
            .try_into()
            .map_err(|_| PrivacyPassError::InvalidTokenResponse)?;

        tokens.push(PrivacyToken::new(
            pending.nonce,
            pending.challenge_digest,
            pending.token_key_id,
            authenticator,
        ));
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_parsing_reads_the_key_and_rejects_extra_challenges() {
        let challenge = URL_SAFE.encode([0xf9, 0x1a, 0, 0]);
        let key =
            URL_SAFE.encode(Ristretto255::serialize_elem(Ristretto255::base_elem()).as_slice());
        let header = HeaderValue::from_str(&format!(
            "PrivateToken challenge={challenge}, token-key={key}, max-age=60"
        ))
        .expect("the test header is valid");
        let parsed = parse_challenge(&header).expect("the challenge parses");

        assert_eq!(parsed.serialized, vec![0xf9, 0x1a, 0, 0]);

        let duplicate = HeaderValue::from_str(&format!(
            "PrivateToken challenge={challenge}, token-key={key}, PrivateToken challenge={challenge}, token-key={key}"
        ))
        .expect("the test header is valid");

        assert!(matches!(
            parse_challenge(&duplicate),
            Err(PrivacyPassError::ChallengeCount(2))
        ));
    }

    #[test]
    fn token_request_and_response_wire_formats_are_stable() {
        let request = TokenRequest {
            truncated_token_key_id: 9,
            blinded_elements: vec![[1; ELEMENT_LEN], [2; ELEMENT_LEN]],
        };
        let bytes = serialize_token_request(&request).expect("the request serializes");

        assert_eq!(&bytes[..2], &TOKEN_TYPE.to_be_bytes());
        assert_eq!(bytes[2], 9);
        assert_eq!(
            &bytes[3..5],
            &u16::try_from(2 * ELEMENT_LEN)
                .expect("the test vector fits")
                .to_be_bytes()
        );
        assert_eq!(bytes.len(), 5 + 2 * ELEMENT_LEN);

        let mut response = Vec::new();
        response.extend_from_slice(
            &u16::try_from(ELEMENT_LEN)
                .expect("the test element fits")
                .to_be_bytes(),
        );
        response.extend_from_slice(&[3; ELEMENT_LEN]);
        response.extend_from_slice(&[4; PROOF_LEN]);
        let parsed = parse_token_response(&response).expect("the response parses");

        assert_eq!(parsed.evaluated_elements, vec![[3; ELEMENT_LEN]]);
        assert_eq!(parsed.proof, [4; PROOF_LEN]);
    }
}
