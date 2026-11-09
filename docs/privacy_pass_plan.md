# Kagi Privacy Pass implementation plan

## Goal

Add an opt-in Privacy Pass mode to the local `kagi` crate. Searches in this mode redeem
anonymous Kagi Privacy Pass tokens instead of sending the session token, session cookies, or
`X-Kagi-Authorization`. Issuance still needs an authenticated Kagi session, but that session is
used only for the issuance endpoints.

## Protocol findings

Kagi uses the batched VOPRF token format from the older RFC 9578 drafts, not the newer generic
token types published in `privacypass` 0.2.0-pre.1 and later.

- Token type: `0xF91A` (batched Ristretto255 tokens).
- VOPRF ciphersuite: `ristretto255-SHA512` from `voprf` 0.5.
- `TokenChallenge`: TLS-serialized, URL-safe base64 in `WWW-Authenticate`.
- `token-key`: TLS-serialized Ristretto255 public key, URL-safe base64.
- Challenge digest: SHA-256 over the raw serialized `TokenChallenge` bytes.
- Token key ID: SHA-256 over the serialized public key.
- Token input: `0xF91A || nonce[32] || challenge_digest[32] || token_key_id[32]`.
- Token request: `token_type:u16 || truncated_token_key_id:u8 || len:u16 || blinded_element[32]...`.
- Token response: `len:u16 || evaluated_element[32]... || proof[64]`.
- Token: `token_type:u16 || nonce[32] || challenge_digest[32] || token_key_id[32] || authenticator[64]`.

The `kagippcore` reference library wraps these structures in `privacypass` at git revision
`35207d3`. That revision is wire-compatible with the older `privacypass` 0.2.0-pre.0 release,
but it also pulls in `blind-rsa-signatures`/`rsa` for token types this client never uses. To keep
the dependency set to the actual cryptographic primitives and to stay within the repository's
crates.io-only source policy, this implementation uses `voprf`, `sha2`, `rand` and `base64`
directly and implements the small fixed-size wire formats in this crate.

### Token lifecycle

1. `GET /pp/wwwa` with `X-Kagi-PrivacyPass-Client: true`.
2. Read `WWW-Authenticate: PrivateToken challenge=..., token-key=...`.
3. Blind `tokens_to_stash` random nonces and send the batched request to `POST /pp/gettokens`
   with:
   - `Content-Type: application/private-token-request`
   - `Authorization: <kagi_session token>`
   - `X-Kagi-PrivacyPass-Client: true`
4. Verify the VOPRF proof, unblind the evaluated elements, and store the resulting tokens.
5. Redeem one token per Kagi request:
   - `Authorization: PrivateToken token="<URL-safe base64 token>"`
   - `X-Kagi-PrivacyPass-Client: true`
   - no `Cookie`, no `X-Kagi-Authorization`, no `Referer`.

The extension requests 300 tokens per batch, keeps at most 2000, and replenishes below 50. The
client mirrors those defaults and triggers a background refill below the low-water mark.

## New public API

- `PrivacyPassOptions` — batch size, store capacity, and low-water mark.
- `PrivacyPassClient` — challenge fetch, issuance, token store, and redemption header building.
- `TokenStore` — thread-safe (`Send + Sync`) FIFO store of unspent tokens with capacity and
  low-water helpers.
- `PrivacyToken` — one unspent token; `Debug` is redacted and
  `authorization_header()` produces the redeemed header.
- `PrivacyPassError` — issuance/redemption errors, wrapped by `kagi::Error::PrivacyPass`.
- `ClientOptions::privacy_pass: Option<PrivacyPassOptions>` — opt-in configuration.
- `Client::with_privacy_pass(token)` — convenience constructor with the defaults.

## Integration touchpoints

- `Client::with_token_and_options` builds one `PrivacyPassClient` when the option is set. It uses
  a dedicated cookie-free `reqwest::Client`.
- `Client::stream_body` branches to a Privacy Pass path before acquiring a session. The path:
  1. takes a token and loads the target document (with a privacy token) to recover its
     `sse_nonce`,
  2. takes a second token and opens `/socket/{search,images}` with that nonce,
  3. retries once with a fresh batch if Kagi rejects the token with 401/403.
- The existing session path and its tests are unchanged.
- Issuance is serialized with an async mutex; the token store is protected by a synchronous
  mutex that is never held across an await point.
- Background replenishment is best-effort and never blocks a request that already has a token.

## Service integration

`neuradex` enables Privacy Pass by default when it builds the Kagi client. The
`KAGI_PRIVACY_PASS` environment variable accepts a boolean and can disable it to fall back to
session-cookie search authentication. The Helm chart exposes the same switch as
`privacyPass` (default `true`). The session token remains required for token issuance.

## Testing

- Unit tests for the token store, header formatting, challenge parsing, and redacted `Debug`.
- A shared `test_support::privacy_pass::PrivacyPassHarness` with:
  - a local VOPRF issuer that produces real challenges, token responses, and proof verification,
  - `httpmock` endpoints for `/pp/wwwa`, `/pp/gettokens`, the search document, and the stream,
  - dynamic responders that redeem and validate the `Authorization` header of both search
    requests, so a token that is not correctly blinded/unblinded fails the test.
- Integration tests run the real client over HTTP and assert that:
  - issuance and both redemptions happen,
  - no cookie or session authorization header is sent,
  - a batch is replenished when the store runs out.

## Non-goals

- Persistent token storage (tokens live in memory for the client's lifetime).
- The `.onion` issuer/origin.
