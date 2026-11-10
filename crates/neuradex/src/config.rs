//! Service configuration read from the environment.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use neuradex_content::{
    CasPolicy, DEFAULT_COMPRESSION_LEVEL, DEFAULT_LARGE_BODY_THRESHOLD_BYTES,
    DEFAULT_PACK_TARGET_BYTES, DEFAULT_RETENTION_DAYS, PolicyError,
};
use secrecy::SecretString;

/// The environment variable holding the listen address.
const LISTEN_ADDR_ENV: &str = "LISTEN_ADDR";

/// The environment variable holding the `User-Agent` header sent with requests.
const USER_AGENT_ENV: &str = "USER_AGENT";

/// The environment variable holding the maximum number of simultaneous searches.
const KAGI_MAX_SESSIONS_ENV: &str = "KAGI_MAX_SESSIONS";

/// The environment variable selecting anonymous Privacy Pass search authentication.
const KAGI_PRIVACY_PASS_ENV: &str = "KAGI_PRIVACY_PASS";

/// The environment variable overriding the persisted Privacy Pass token state file.
const KAGI_PRIVACY_PASS_STATE_ENV: &str = "KAGI_PRIVACY_PASS_STATE";

/// The XDG state directory environment variable.
const XDG_STATE_HOME_ENV: &str = "XDG_STATE_HOME";

/// The home directory environment variable.
const HOME_ENV: &str = "HOME";

/// The environment variable holding the Kagi session token.
const KAGI_TOKEN_ENV: &str = "KAGI_SESSION_TOKEN";

/// The environment variable holding the JSONL audit log path.
const AUDIT_LOG_PATH_ENV: &str = "AUDIT_LOG_PATH";

/// The environment variable holding the header HMAC key.
const AUDIT_HMAC_KEY_ENV: &str = "AUDIT_HMAC_KEY";

/// The environment variable selecting the content store: `off`, `local`, or `s3`.
const CONTENT_STORE_ENV: &str = "CONTENT_STORE";

/// The environment variable holding the local content store root.
const CONTENT_LOCAL_PATH_ENV: &str = "CONTENT_LOCAL_PATH";

/// The environment variable holding the S3 endpoint URL.
const CONTENT_S3_ENDPOINT_ENV: &str = "CONTENT_S3_ENDPOINT";

/// The environment variable holding the S3 signing region.
const CONTENT_S3_REGION_ENV: &str = "CONTENT_S3_REGION";

/// The environment variable holding the S3 bucket.
const CONTENT_S3_BUCKET_ENV: &str = "CONTENT_S3_BUCKET";

/// The optional environment variable holding the S3 key prefix.
const CONTENT_S3_PREFIX_ENV: &str = "CONTENT_S3_PREFIX";

/// The environment variable holding the S3 access key identifier.
const CONTENT_S3_ACCESS_KEY_ID_ENV: &str = "CONTENT_S3_ACCESS_KEY_ID";

/// The environment variable holding the S3 secret access key.
const CONTENT_S3_SECRET_ACCESS_KEY_ENV: &str = "CONTENT_S3_SECRET_ACCESS_KEY";

/// The environment variable selecting S3 path-style addressing.
const CONTENT_S3_FORCE_PATH_STYLE_ENV: &str = "CONTENT_S3_FORCE_PATH_STYLE";

/// The environment variable holding the content pack target size.
const CONTENT_PACK_TARGET_BYTES_ENV: &str = "CONTENT_PACK_TARGET_BYTES";

/// The environment variable holding the maximum age of an open content pack.
const CONTENT_MAX_PACK_AGE_SECS_ENV: &str = "CONTENT_MAX_PACK_AGE_SECS";

/// The environment variable holding the maximum captured body size.
const CONTENT_MAX_BODY_BYTES_ENV: &str = "CONTENT_MAX_BODY_BYTES";

/// The environment variable holding the content retention period.
const CONTENT_RETENTION_DAYS_ENV: &str = "CONTENT_RETENTION_DAYS";

/// The environment variable holding the content compression level.
const CONTENT_COMPRESSION_LEVEL_ENV: &str = "CONTENT_COMPRESSION_LEVEL";

/// The default listen address.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";

/// The default maximum number of request body bytes captured for the content pipeline.
pub(crate) const DEFAULT_CONTENT_MAX_BODY_BYTES: usize = 25 * 1024 * 1024;

/// The default maximum time an open content pack may go unsealed, in seconds.
const DEFAULT_CONTENT_MAX_PACK_AGE_SECS: u64 = 600;

/// The largest accepted `KAGI_MAX_SESSIONS` value.
///
/// Searches are network-bound, so a small ceiling is enough; rejecting absurd values keeps an
/// operator typo from reaching `Semaphore::new`, which panics above Tokio's permit limit.
pub(crate) const MAX_SEARCH_SESSIONS: usize = 64;

/// The configuration of the content plane.
#[derive(Clone, Debug)]
pub struct ContentConfig {
    backend: ContentBackend,
    policy: CasPolicy,
}

impl ContentConfig {
    /// Returns the configured content backend.
    #[must_use]
    pub const fn backend(&self) -> &ContentBackend {
        &self.backend
    }

    /// Returns the validated packing and retention policy.
    #[must_use]
    pub const fn policy(&self) -> &CasPolicy {
        &self.policy
    }

    /// Whether a content store is configured.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        !matches!(self.backend, ContentBackend::Disabled)
    }

    /// Returns the maximum captured body size in bytes.
    #[must_use]
    pub fn max_body_bytes(&self) -> usize {
        usize::try_from(self.policy.max_captured_body_bytes()).unwrap_or(usize::MAX)
    }
}

/// The content store selected by `CONTENT_STORE`.
#[derive(Clone, Debug)]
pub enum ContentBackend {
    /// Content storage is disabled.
    Disabled,
    /// Content is stored under a local directory.
    Local {
        /// The root directory of the local store.
        path: PathBuf,
    },
    /// Content is stored in an S3-compatible bucket.
    S3(S3ContentSettings),
}

/// The S3 connection settings, with both credentials kept secret.
#[derive(Clone, Debug)]
pub struct S3ContentSettings {
    endpoint: String,
    region: String,
    bucket: String,
    prefix: Option<String>,
    access_key_id: SecretString,
    secret_access_key: SecretString,
    force_path_style: bool,
}

impl S3ContentSettings {
    /// Returns the S3 endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the signing region.
    #[must_use]
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Returns the bucket name.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Returns the optional key prefix, normalized without surrounding slashes.
    #[must_use]
    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// Returns the access key identifier.
    #[must_use]
    pub const fn access_key_id(&self) -> &SecretString {
        &self.access_key_id
    }

    /// Returns the secret access key.
    #[must_use]
    pub const fn secret_access_key(&self) -> &SecretString {
        &self.secret_access_key
    }

    /// Whether requests use path-style addressing.
    #[must_use]
    pub const fn force_path_style(&self) -> bool {
        self.force_path_style
    }
}

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
    /// The optional file holding unspent Privacy Pass tokens across restarts.
    privacy_pass_state: Option<PathBuf>,
    /// The JSONL audit log path; `None` writes audit events to stdout.
    audit_log_path: Option<PathBuf>,
    /// The optional HMAC key used to hash identifiers and IP addresses in audit headers.
    audit_hmac_key: Option<SecretString>,
    /// The content plane configuration.
    pub content: ContentConfig,
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

    /// Returns the JSONL audit log path, or `None` when audit events go to stdout.
    #[must_use]
    pub fn audit_log_path(&self) -> Option<&Path> {
        self.audit_log_path.as_deref()
    }

    /// Returns the optional HMAC key for identifiers and IP addresses in audit headers.
    #[must_use]
    pub const fn audit_hmac_key(&self) -> Option<&SecretString> {
        self.audit_hmac_key.as_ref()
    }

    /// Returns the file used to persist unspent Privacy Pass tokens.
    ///
    /// `None` keeps tokens in memory only, which also happens when neither `XDG_STATE_HOME`
    /// nor `HOME` is available and no override is set.
    #[must_use]
    pub fn privacy_pass_state(&self) -> Option<&Path> {
        self.privacy_pass_state.as_deref()
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

        let privacy_pass_state = parse_privacy_pass_state(&get);

        let kagi_token = get(KAGI_TOKEN_ENV)
            .filter(|token| !token.is_empty())
            .map(SecretString::from)
            .ok_or(ConfigError::MissingToken)?;

        let audit_log_path = get(AUDIT_LOG_PATH_ENV)
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty() && path != "-")
            .map(PathBuf::from);

        let audit_hmac_key = get(AUDIT_HMAC_KEY_ENV)
            .filter(|key| !key.is_empty())
            .map(SecretString::from);

        let content = parse_content_config(&get)?;

        Ok(Self {
            listen_addr,
            user_agent,
            kagi_token,
            max_sessions,
            privacy_pass,
            privacy_pass_state,
            audit_log_path,
            audit_hmac_key,
            content,
        })
    }
}

/// Resolves the file used to persist unspent Privacy Pass tokens.
///
/// `KAGI_PRIVACY_PASS_STATE` overrides the path; `off`, `none`, and `-` disable persistence.
/// Otherwise the XDG state directory is used: `${XDG_STATE_HOME:-$HOME/.local/state}` followed
/// by `neuradex/privacy-pass/tokens.json`. Relative XDG paths are ignored as the XDG base
/// directory specification requires, and a missing `HOME` leaves the store in memory.
fn parse_privacy_pass_state(get: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(value) = get(KAGI_PRIVACY_PASS_STATE_ENV) {
        let value = value.trim();

        if !value.is_empty() {
            return match value.to_ascii_lowercase().as_str() {
                "off" | "none" | "-" => None,
                _ => Some(PathBuf::from(value)),
            };
        }
    }

    let state_home = absolute_env_path(get, XDG_STATE_HOME_ENV).or_else(|| {
        absolute_env_path(get, HOME_ENV).map(|home| home.join(".local").join("state"))
    })?;

    Some(
        state_home
            .join("neuradex")
            .join("privacy-pass")
            .join("tokens.json"),
    )
}

/// Returns an environment path when it is set and absolute.
fn absolute_env_path(get: &impl Fn(&str) -> Option<String>, name: &'static str) -> Option<PathBuf> {
    get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Parses the content plane configuration and validates the resulting policy.
fn parse_content_config(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<ContentConfig, ConfigError> {
    let backend = match get(CONTENT_STORE_ENV) {
        None => ContentBackend::Disabled,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "off" => ContentBackend::Disabled,
            "local" => ContentBackend::Local {
                path: PathBuf::from(content_required(get, "local", CONTENT_LOCAL_PATH_ENV)?),
            },
            "s3" => ContentBackend::S3(parse_s3_settings(get)?),
            _ => return Err(ConfigError::InvalidContentStore(value)),
        },
    };

    let pack_target_bytes = content_positive_u64(
        get,
        CONTENT_PACK_TARGET_BYTES_ENV,
        DEFAULT_PACK_TARGET_BYTES,
    )?;
    let max_pack_age_secs = content_positive_u64(
        get,
        CONTENT_MAX_PACK_AGE_SECS_ENV,
        DEFAULT_CONTENT_MAX_PACK_AGE_SECS,
    )?;
    let max_body_bytes = match get(CONTENT_MAX_BODY_BYTES_ENV) {
        Some(value) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| ConfigError::InvalidContentMaxBodyBytes(value.clone()))?,
        None => DEFAULT_CONTENT_MAX_BODY_BYTES,
    };
    let retention_days =
        content_positive_u32(get, CONTENT_RETENTION_DAYS_ENV, DEFAULT_RETENTION_DAYS)?;
    let compression_level = content_i32(
        get,
        CONTENT_COMPRESSION_LEVEL_ENV,
        DEFAULT_COMPRESSION_LEVEL,
    )?;

    let policy = CasPolicy::new(
        pack_target_bytes,
        Duration::from_secs(max_pack_age_secs),
        u64::try_from(max_body_bytes).unwrap_or(u64::MAX),
        DEFAULT_LARGE_BODY_THRESHOLD_BYTES,
        compression_level,
        retention_days,
    )?;

    Ok(ContentConfig { backend, policy })
}

/// Parses the S3 settings required by `CONTENT_STORE=s3`.
fn parse_s3_settings(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<S3ContentSettings, ConfigError> {
    let endpoint = content_required(get, "s3", CONTENT_S3_ENDPOINT_ENV)?;
    let region = content_required(get, "s3", CONTENT_S3_REGION_ENV)?;
    let bucket = content_required(get, "s3", CONTENT_S3_BUCKET_ENV)?;
    let access_key_id =
        SecretString::from(content_required(get, "s3", CONTENT_S3_ACCESS_KEY_ID_ENV)?);
    let secret_access_key = SecretString::from(content_required(
        get,
        "s3",
        CONTENT_S3_SECRET_ACCESS_KEY_ENV,
    )?);
    let prefix = get(CONTENT_S3_PREFIX_ENV)
        .map(|value| value.trim().trim_matches('/').to_owned())
        .filter(|value| !value.is_empty());
    let force_path_style = match get(CONTENT_S3_FORCE_PATH_STYLE_ENV) {
        Some(value) => parse_bool(&value).ok_or_else(|| ConfigError::InvalidContentValue {
            name: CONTENT_S3_FORCE_PATH_STYLE_ENV,
            expected: "a boolean",
            value: value.clone(),
        })?,
        None => true,
    };

    Ok(S3ContentSettings {
        endpoint,
        region,
        bucket,
        prefix,
        access_key_id,
        secret_access_key,
        force_path_style,
    })
}

/// Returns a required, non-empty content setting.
fn content_required(
    get: &impl Fn(&str) -> Option<String>,
    store: &'static str,
    name: &'static str,
) -> Result<String, ConfigError> {
    get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::MissingContentSetting { store, name })
}

/// Parses an optional positive `u64` content setting.
fn content_positive_u64(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    get(name).map_or(Ok(default), |value| {
        value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| ConfigError::InvalidContentValue {
                name,
                expected: "a positive integer",
                value: value.clone(),
            })
    })
}

/// Parses an optional positive `u32` content setting.
fn content_positive_u32(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: u32,
) -> Result<u32, ConfigError> {
    get(name).map_or(Ok(default), |value| {
        value
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| ConfigError::InvalidContentValue {
                name,
                expected: "a positive integer",
                value: value.clone(),
            })
    })
}

/// Parses an optional `i32` content setting.
fn content_i32(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: i32,
) -> Result<i32, ConfigError> {
    get(name).map_or(Ok(default), |value| {
        value
            .trim()
            .parse::<i32>()
            .map_err(|_| ConfigError::InvalidContentValue {
                name,
                expected: "an integer",
                value: value.clone(),
            })
    })
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
    /// The content store selection is not one of the accepted names.
    #[error("invalid CONTENT_STORE: expected off, local, or s3, got {0:?}")]
    InvalidContentStore(String),
    /// A content setting required by the selected store is missing or empty.
    #[error("CONTENT_STORE={store} requires {name}")]
    MissingContentSetting {
        /// The selected store.
        store: &'static str,
        /// The missing environment variable.
        name: &'static str,
    },
    /// A content setting has an invalid value.
    #[error("invalid {name}: expected {expected}, got {value:?}")]
    InvalidContentValue {
        /// The environment variable.
        name: &'static str,
        /// A description of the accepted value.
        expected: &'static str,
        /// The rejected value.
        value: String,
    },
    /// The body-capture limit is not a positive integer.
    #[error("invalid CONTENT_MAX_BODY_BYTES: expected a positive integer, got {0:?}")]
    InvalidContentMaxBodyBytes(String),
    /// The validated content policy rejects a configured limit.
    #[error("invalid content policy: {0}")]
    InvalidContentPolicy(#[from] PolicyError),
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

    /// The complete environment of a valid S3 content store.
    fn s3_vars() -> [(&'static str, &'static str); 7] {
        [
            (KAGI_TOKEN_ENV, "token"),
            (CONTENT_STORE_ENV, "s3"),
            (CONTENT_S3_ENDPOINT_ENV, "https://s3.example.test"),
            (CONTENT_S3_REGION_ENV, "us-west-004"),
            (CONTENT_S3_BUCKET_ENV, "neuradex"),
            (CONTENT_S3_ACCESS_KEY_ID_ENV, "access-key-id"),
            (CONTENT_S3_SECRET_ACCESS_KEY_ENV, "super-secret-value"),
        ]
    }

    #[test]
    fn uses_the_defaults() {
        let config = parse_env(&[(KAGI_TOKEN_ENV, "token")]).expect("valid config");

        assert_eq!(config.listen_addr, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(config.user_agent, kagi::USER_AGENT);
        assert_eq!(config.max_sessions, kagi::DEFAULT_MAX_SESSIONS);
        assert!(config.privacy_pass);
        assert_eq!(config.kagi_token().expose_secret(), "token");
        assert_eq!(config.privacy_pass_state(), None);
        assert_eq!(config.audit_log_path(), None);
        assert!(config.audit_hmac_key().is_none());
        assert!(matches!(config.content.backend(), ContentBackend::Disabled));
        assert!(!config.content.is_enabled());
        assert_eq!(
            config.content.max_body_bytes(),
            DEFAULT_CONTENT_MAX_BODY_BYTES
        );
        assert_eq!(
            config.content.policy().pack_target_bytes(),
            DEFAULT_PACK_TARGET_BYTES
        );
        assert_eq!(
            config.content.policy().max_pack_age(),
            Duration::from_secs(DEFAULT_CONTENT_MAX_PACK_AGE_SECS)
        );
        assert_eq!(
            config.content.policy().retention_days(),
            DEFAULT_RETENTION_DAYS
        );
        assert_eq!(
            config.content.policy().compression_level(),
            DEFAULT_COMPRESSION_LEVEL
        );
    }

    #[test]
    fn reads_valid_overrides() {
        let config = parse_env(&[
            (LISTEN_ADDR_ENV, "0.0.0.0:9000"),
            (USER_AGENT_ENV, "custom-agent/1.0"),
            (KAGI_MAX_SESSIONS_ENV, " 4 "),
            (KAGI_PRIVACY_PASS_ENV, "off"),
            (KAGI_TOKEN_ENV, "secret"),
            (AUDIT_LOG_PATH_ENV, " /var/log/neuradex/audit.jsonl "),
            (AUDIT_HMAC_KEY_ENV, "hmac-secret"),
            (CONTENT_STORE_ENV, "local"),
            (CONTENT_LOCAL_PATH_ENV, " /var/lib/neuradex/content "),
            (CONTENT_PACK_TARGET_BYTES_ENV, "1048576"),
            (CONTENT_MAX_PACK_AGE_SECS_ENV, "30"),
            (CONTENT_MAX_BODY_BYTES_ENV, " 1048576 "),
            (CONTENT_RETENTION_DAYS_ENV, "7"),
            (CONTENT_COMPRESSION_LEVEL_ENV, "5"),
        ])
        .expect("valid config");

        assert_eq!(config.listen_addr, SocketAddr::from(([0, 0, 0, 0], 9000)));
        assert_eq!(config.user_agent, "custom-agent/1.0");
        assert_eq!(config.max_sessions, 4);
        assert!(!config.privacy_pass);
        assert_eq!(config.kagi_token().expose_secret(), "secret");
        assert_eq!(
            config.audit_log_path(),
            Some(Path::new("/var/log/neuradex/audit.jsonl"))
        );
        assert_eq!(
            config.audit_hmac_key().map(ExposeSecret::expose_secret),
            Some("hmac-secret")
        );
        assert!(matches!(
            config.content.backend(),
            ContentBackend::Local { path }
                if path == Path::new("/var/lib/neuradex/content")
        ));
        assert_eq!(config.content.max_body_bytes(), 1_048_576);
        assert_eq!(config.content.policy().pack_target_bytes(), 1_048_576);
        assert_eq!(
            config.content.policy().max_pack_age(),
            Duration::from_secs(30)
        );
        assert_eq!(config.content.policy().retention_days(), 7);
        assert_eq!(config.content.policy().compression_level(), 5);
    }

    #[test]
    fn parses_the_s3_content_store() {
        let mut vars = s3_vars().to_vec();
        vars.push((CONTENT_S3_PREFIX_ENV, " /tenant/ "));
        vars.push((CONTENT_S3_FORCE_PATH_STYLE_ENV, "false"));

        let config = parse_env(&vars).expect("valid config");
        let ContentBackend::S3(settings) = config.content.backend() else {
            panic!("expected the S3 backend");
        };

        assert_eq!(settings.endpoint(), "https://s3.example.test");
        assert_eq!(settings.region(), "us-west-004");
        assert_eq!(settings.bucket(), "neuradex");
        assert_eq!(settings.prefix(), Some("tenant"));
        assert_eq!(settings.access_key_id().expose_secret(), "access-key-id");
        assert_eq!(
            settings.secret_access_key().expose_secret(),
            "super-secret-value"
        );
        assert!(!settings.force_path_style());
    }

    #[test]
    fn s3_defaults_to_path_style_addressing() {
        let config = parse_env(&s3_vars()).expect("valid config");
        let ContentBackend::S3(settings) = config.content.backend() else {
            panic!("expected the S3 backend");
        };

        assert!(settings.force_path_style());
        assert_eq!(settings.prefix(), None);
    }

    #[test]
    fn uses_the_xdg_state_home_for_privacy_pass_tokens() {
        let config = parse_env(&[
            (KAGI_TOKEN_ENV, "token"),
            (XDG_STATE_HOME_ENV, " /var/lib/state "),
        ])
        .expect("valid config");

        assert_eq!(
            config.privacy_pass_state(),
            Some(Path::new(
                "/var/lib/state/neuradex/privacy-pass/tokens.json"
            ))
        );
    }

    #[test]
    fn falls_back_to_home_for_privacy_pass_tokens() {
        let config = parse_env(&[
            (KAGI_TOKEN_ENV, "token"),
            (HOME_ENV, "/home/tester"),
            // A relative XDG path must be ignored in favour of HOME.
            (XDG_STATE_HOME_ENV, "relative/state"),
        ])
        .expect("valid config");

        assert_eq!(
            config.privacy_pass_state(),
            Some(Path::new(
                "/home/tester/.local/state/neuradex/privacy-pass/tokens.json"
            ))
        );
    }

    #[test]
    fn overrides_the_privacy_pass_state_path() {
        let config = parse_env(&[
            (KAGI_TOKEN_ENV, "token"),
            (HOME_ENV, "/home/tester"),
            (KAGI_PRIVACY_PASS_STATE_ENV, " /tmp/neuradex/tokens.json "),
        ])
        .expect("valid config");

        assert_eq!(
            config.privacy_pass_state(),
            Some(Path::new("/tmp/neuradex/tokens.json"))
        );
    }

    #[test]
    fn can_disable_privacy_pass_state_persistence() {
        for value in ["off", "none", "-"] {
            let config = parse_env(&[
                (KAGI_TOKEN_ENV, "token"),
                (HOME_ENV, "/home/tester"),
                (KAGI_PRIVACY_PASS_STATE_ENV, value),
            ])
            .expect("valid config");

            assert_eq!(config.privacy_pass_state(), None, "{value:?}");
        }
    }

    #[test]
    fn keeps_privacy_pass_tokens_in_memory_without_a_state_home() {
        let config = parse_env(&[
            (KAGI_TOKEN_ENV, "token"),
            (XDG_STATE_HOME_ENV, "relative/state"),
        ])
        .expect("valid config");

        assert_eq!(config.privacy_pass_state(), None);
    }

    #[test]
    fn rejects_a_local_store_without_a_path() {
        let error = parse_env(&[(KAGI_TOKEN_ENV, "token"), (CONTENT_STORE_ENV, "local")])
            .expect_err("missing local path");

        assert!(matches!(
            error,
            ConfigError::MissingContentSetting {
                store: "local",
                name: CONTENT_LOCAL_PATH_ENV,
            }
        ));
        assert_eq!(
            error.to_string(),
            "CONTENT_STORE=local requires CONTENT_LOCAL_PATH"
        );
    }

    #[test]
    fn rejects_missing_s3_settings() {
        let required = [
            CONTENT_S3_ENDPOINT_ENV,
            CONTENT_S3_REGION_ENV,
            CONTENT_S3_BUCKET_ENV,
            CONTENT_S3_ACCESS_KEY_ID_ENV,
            CONTENT_S3_SECRET_ACCESS_KEY_ENV,
        ];

        for missing in required {
            let vars: Vec<(&str, &str)> = s3_vars()
                .into_iter()
                .filter(|(name, _)| *name != missing)
                .collect();

            let error = parse_env(&vars).expect_err("missing S3 setting");

            assert!(
                matches!(
                    error,
                    ConfigError::MissingContentSetting {
                        store: "s3",
                        name,
                    } if name == missing
                ),
                "unexpected error for {missing}: {error}"
            );
            assert_eq!(
                error.to_string(),
                format!("CONTENT_STORE=s3 requires {missing}")
            );
        }
    }

    #[test]
    fn rejects_invalid_content_store_names() {
        let error = parse_env(&[(KAGI_TOKEN_ENV, "token"), (CONTENT_STORE_ENV, "nope")])
            .expect_err("invalid store");

        assert!(matches!(error, ConfigError::InvalidContentStore(_)));
        assert_eq!(
            error.to_string(),
            r#"invalid CONTENT_STORE: expected off, local, or s3, got "nope""#
        );
    }

    #[test]
    fn rejects_invalid_content_values() {
        for (name, value) in [
            (CONTENT_PACK_TARGET_BYTES_ENV, "0"),
            (CONTENT_PACK_TARGET_BYTES_ENV, "many"),
            (CONTENT_MAX_PACK_AGE_SECS_ENV, "0"),
            (CONTENT_RETENTION_DAYS_ENV, "-1"),
            (CONTENT_COMPRESSION_LEVEL_ENV, "high"),
        ] {
            let error = parse_env(&[(KAGI_TOKEN_ENV, "token"), (name, value)])
                .expect_err("invalid content value");

            assert!(
                matches!(
                    error,
                    ConfigError::InvalidContentValue {
                        name: error_name,
                        value: ref error_value,
                        ..
                    } if error_name == name && error_value == value
                ),
                "unexpected error for {name}={value}: {error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_content_booleans() {
        let mut vars = s3_vars().to_vec();
        vars.push((CONTENT_S3_FORCE_PATH_STYLE_ENV, "maybe"));

        let error = parse_env(&vars).expect_err("invalid boolean");

        assert!(matches!(
            error,
            ConfigError::InvalidContentValue {
                name: CONTENT_S3_FORCE_PATH_STYLE_ENV,
                ..
            }
        ));
    }

    #[test]
    fn rejects_invalid_content_policies() {
        let error = parse_env(&[
            (KAGI_TOKEN_ENV, "token"),
            (CONTENT_COMPRESSION_LEVEL_ENV, "2147483647"),
        ])
        .expect_err("invalid compression level");

        assert!(matches!(error, ConfigError::InvalidContentPolicy(_)));
    }

    #[test]
    fn redacts_content_secrets() {
        let config = parse_env(&s3_vars()).expect("valid config");
        let rendered = format!("{config:?}");

        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(!rendered.contains("access-key-id"), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
    }

    #[test]
    fn treats_dash_and_empty_audit_paths_as_stdout() {
        for path in ["-", "", " - "] {
            let config = parse_env(&[(KAGI_TOKEN_ENV, "token"), (AUDIT_LOG_PATH_ENV, path)])
                .expect("valid config");

            assert_eq!(config.audit_log_path(), None, "{path:?}");
        }
    }

    #[test]
    fn treats_an_empty_hmac_key_as_disabled() {
        let config = parse_env(&[(KAGI_TOKEN_ENV, "token"), (AUDIT_HMAC_KEY_ENV, "")])
            .expect("valid config");

        assert!(config.audit_hmac_key().is_none());
    }

    #[test]
    fn rejects_invalid_body_capture_limits() {
        for value in ["0", "-1", "many", ""] {
            let error = parse_env(&[
                (KAGI_TOKEN_ENV, "token"),
                (CONTENT_MAX_BODY_BYTES_ENV, value),
            ])
            .expect_err("invalid body capture limit");

            assert!(
                matches!(error, ConfigError::InvalidContentMaxBodyBytes(_)),
                "{value:?}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "invalid CONTENT_MAX_BODY_BYTES: expected a positive integer, got {value:?}"
                )
            );
        }
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
