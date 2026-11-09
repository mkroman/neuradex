//! Service configuration read from the environment.

use std::net::SocketAddr;

use secrecy::SecretString;

/// The environment variable holding the listen address.
const LISTEN_ADDR_ENV: &str = "LISTEN_ADDR";

/// The environment variable holding the `User-Agent` header sent with requests.
const USER_AGENT_ENV: &str = "USER_AGENT";

/// The environment variable holding the maximum number of simultaneous searches.
const KAGI_MAX_SESSIONS_ENV: &str = "KAGI_MAX_SESSIONS";

/// The environment variable selecting anonymous Privacy Pass search authentication.
const KAGI_PRIVACY_PASS_ENV: &str = "KAGI_PRIVACY_PASS";

/// The environment variable holding the Kagi session token.
const KAGI_TOKEN_ENV: &str = "KAGI_SESSION_TOKEN";

/// The default listen address.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";

/// The largest accepted `KAGI_MAX_SESSIONS` value.
///
/// Searches are network-bound, so a small ceiling is enough; rejecting absurd values keeps an
/// operator typo from reaching `Semaphore::new`, which panics above Tokio's permit limit.
pub(crate) const MAX_SEARCH_SESSIONS: usize = 64;

/// The configuration of the HTTP service.
#[derive(Debug)]
pub struct Config {
    /// The address the server binds to.
    pub listen_addr: SocketAddr,
    /// The `User-Agent` header sent with requests.
    pub user_agent: String,
    /// The Kagi session token.
    kagi_token: SecretString,
    /// The maximum number of simultaneous searches.
    pub max_sessions: usize,
    /// Whether searches authenticate with anonymous Privacy Pass tokens.
    pub privacy_pass: bool,
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns an error if a variable is set to an invalid value or the Kagi session token is
    /// missing or empty.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::parse(|name| std::env::var(name).ok())
    }

    /// Returns the Kagi session token.
    #[must_use]
    pub const fn kagi_token(&self) -> &SecretString {
        &self.kagi_token
    }

    /// Parses the configuration from `get`, a lookup of environment values by name.
    fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let listen_addr = match get(LISTEN_ADDR_ENV) {
            Some(value) => match value.parse() {
                Ok(addr) => addr,
                Err(_) => return Err(ConfigError::InvalidListenAddr(value)),
            },
            None => DEFAULT_LISTEN_ADDR
                .parse()
                .expect("the default listen address is valid"),
        };

        let user_agent = get(USER_AGENT_ENV).unwrap_or_else(|| kagi::USER_AGENT.to_string());

        let max_sessions = match get(KAGI_MAX_SESSIONS_ENV) {
            Some(value) => value
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|count| (1..=MAX_SEARCH_SESSIONS).contains(count))
                .ok_or_else(|| ConfigError::InvalidMaxSessions(value.clone()))?,
            None => kagi::DEFAULT_MAX_SESSIONS,
        };

        let privacy_pass = match get(KAGI_PRIVACY_PASS_ENV) {
            Some(value) => {
                parse_bool(&value).ok_or_else(|| ConfigError::InvalidPrivacyPass(value.clone()))?
            }
            None => true,
        };

        let kagi_token = get(KAGI_TOKEN_ENV)
            .filter(|token| !token.is_empty())
            .map(SecretString::from)
            .ok_or(ConfigError::MissingToken)?;

        Ok(Self {
            listen_addr,
            user_agent,
            kagi_token,
            max_sessions,
            privacy_pass,
        })
    }
}

/// Errors that can occur while reading the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The listen address is not a valid socket address.
    #[error("invalid LISTEN_ADDR: expected a socket address, got {0:?}")]
    InvalidListenAddr(String),
    /// The maximum session count is outside the accepted range.
    #[error(
        "invalid KAGI_MAX_SESSIONS: expected an integer between 1 and {max}, got {0:?}",
        max = MAX_SEARCH_SESSIONS
    )]
    InvalidMaxSessions(String),
    /// The Privacy Pass setting is not a boolean.
    #[error("invalid KAGI_PRIVACY_PASS: expected a boolean, got {0:?}")]
    InvalidPrivacyPass(String),
    /// The Kagi session token is missing or empty.
    #[error("KAGI_SESSION_TOKEN is required")]
    MissingToken,
}

/// Parses a boolean environment setting.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use secrecy::ExposeSecret;

    use super::*;

    /// Parses a configuration from the given environment pairs.
    fn parse_env(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();

        Config::parse(|name| vars.get(name).map(|value| (*value).to_owned()))
    }

    #[test]
    fn uses_the_defaults() {
        let config = parse_env(&[(KAGI_TOKEN_ENV, "token")]).expect("valid config");

        assert_eq!(config.listen_addr, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(config.user_agent, kagi::USER_AGENT);
        assert_eq!(config.max_sessions, kagi::DEFAULT_MAX_SESSIONS);
        assert!(config.privacy_pass);
        assert_eq!(config.kagi_token().expose_secret(), "token");
    }

    #[test]
    fn reads_valid_overrides() {
        let config = parse_env(&[
            (LISTEN_ADDR_ENV, "0.0.0.0:9000"),
            (USER_AGENT_ENV, "custom-agent/1.0"),
            (KAGI_MAX_SESSIONS_ENV, " 4 "),
            (KAGI_PRIVACY_PASS_ENV, "off"),
            (KAGI_TOKEN_ENV, "secret"),
        ])
        .expect("valid config");

        assert_eq!(config.listen_addr, SocketAddr::from(([0, 0, 0, 0], 9000)));
        assert_eq!(config.user_agent, "custom-agent/1.0");
        assert_eq!(config.max_sessions, 4);
        assert!(!config.privacy_pass);
        assert_eq!(config.kagi_token().expose_secret(), "secret");
    }

    #[test]
    fn rejects_invalid_listen_addresses() {
        let error = parse_env(&[(LISTEN_ADDR_ENV, "nope"), (KAGI_TOKEN_ENV, "token")])
            .expect_err("invalid address");

        assert!(matches!(error, ConfigError::InvalidListenAddr(_)));
        assert_eq!(
            error.to_string(),
            r#"invalid LISTEN_ADDR: expected a socket address, got "nope""#
        );
    }

    #[test]
    fn rejects_invalid_session_counts() {
        for value in ["0", "-1", "many", "", "65", "18446744073709551615"] {
            let error = parse_env(&[(KAGI_MAX_SESSIONS_ENV, value), (KAGI_TOKEN_ENV, "token")])
                .expect_err("invalid session count");

            assert!(
                matches!(error, ConfigError::InvalidMaxSessions(_)),
                "{value:?}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "invalid KAGI_MAX_SESSIONS: expected an integer between 1 and \
                     {MAX_SEARCH_SESSIONS}, got {value:?}"
                )
            );
        }
    }

    #[test]
    fn accepts_the_maximum_session_count() {
        let config = parse_env(&[(KAGI_MAX_SESSIONS_ENV, "64"), (KAGI_TOKEN_ENV, "token")])
            .expect("the upper bound is valid");

        assert_eq!(config.max_sessions, MAX_SEARCH_SESSIONS);
    }

    #[test]
    fn rejects_invalid_booleans() {
        let error = parse_env(&[(KAGI_PRIVACY_PASS_ENV, "maybe"), (KAGI_TOKEN_ENV, "token")])
            .expect_err("invalid boolean");

        assert!(matches!(error, ConfigError::InvalidPrivacyPass(_)));
        assert_eq!(
            error.to_string(),
            r#"invalid KAGI_PRIVACY_PASS: expected a boolean, got "maybe""#
        );
    }

    #[test]
    fn requires_a_non_empty_token() {
        for vars in [&[][..], &[(KAGI_TOKEN_ENV, "")][..]] {
            let error = parse_env(vars).expect_err("missing token");

            assert!(matches!(error, ConfigError::MissingToken));
            assert_eq!(error.to_string(), "KAGI_SESSION_TOKEN is required");
        }
    }

    #[test]
    fn parses_boolean_environment_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " true "] {
            assert_eq!(parse_bool(value), Some(true), "{value:?}");
        }

        for value in ["0", "false", "FALSE", "no", "off", " false "] {
            assert_eq!(parse_bool(value), Some(false), "{value:?}");
        }

        for value in ["", "maybe", "2"] {
            assert_eq!(parse_bool(value), None, "{value:?}");
        }
    }
}
