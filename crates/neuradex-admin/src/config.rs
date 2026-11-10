//! Environment-backed settings shared by the admin commands.

use std::env;

use neuradex_content::S3Config;

use crate::content_lifecycle;

const CONTENT_RETENTION_DAYS: &str = "CONTENT_RETENTION_DAYS";
const CONTENT_S3_PREFIX: &str = "CONTENT_S3_PREFIX";
const CONTENT_S3_ENDPOINT: &str = "CONTENT_S3_ENDPOINT";
const CONTENT_S3_REGION: &str = "CONTENT_S3_REGION";
const CONTENT_S3_BUCKET: &str = "CONTENT_S3_BUCKET";
const CONTENT_S3_ACCESS_KEY_ID: &str = "CONTENT_S3_ACCESS_KEY_ID";
const CONTENT_S3_SECRET_ACCESS_KEY: &str = "CONTENT_S3_SECRET_ACCESS_KEY";
const CONTENT_S3_FORCE_PATH_STYLE: &str = "CONTENT_S3_FORCE_PATH_STYLE";

const DEFAULT_RETENTION_DAYS: u32 = 365;

/// Configuration needed to generate a lifecycle policy.
#[derive(Debug)]
pub struct PolicySettings {
    retention_days: u32,
    prefix: String,
}

impl PolicySettings {
    /// Builds policy settings from an explicit retention window and prefix.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(retention_days: u32, prefix: impl Into<String>) -> Self {
        Self {
            retention_days,
            prefix: normalize_prefix(&prefix.into()),
        }
    }

    /// Reads the policy settings from the environment.
    pub(crate) fn from_env() -> Result<Self, content_lifecycle::Error> {
        let retention_days = match env_value(CONTENT_RETENTION_DAYS) {
            Some(value) => value
                .parse::<u32>()
                .ok()
                .filter(|days| *days > 0)
                .ok_or_else(|| {
                    content_lifecycle::Error::Config(format!(
                        "{CONTENT_RETENTION_DAYS} must be a positive integer (got '{value}')"
                    ))
                })?,
            None => DEFAULT_RETENTION_DAYS,
        };

        let prefix = normalize_prefix(env_value(CONTENT_S3_PREFIX).as_deref().unwrap_or_default());

        Ok(Self {
            retention_days,
            prefix,
        })
    }

    /// Returns the retention window in days.
    pub(crate) const fn retention_days(&self) -> u32 {
        self.retention_days
    }

    /// Returns the normalized content prefix (without leading or trailing slashes).
    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// S3 settings needed to check or apply a lifecycle policy.
#[derive(Debug)]
pub struct S3Settings {
    config: S3Config,
}

impl S3Settings {
    /// Reads the S3 settings from the environment.
    pub(crate) fn from_env(prefix: &str) -> Result<Self, content_lifecycle::Error> {
        let mut config = S3Config::new(
            required(CONTENT_S3_ENDPOINT)?,
            required(CONTENT_S3_REGION)?,
            required(CONTENT_S3_BUCKET)?,
            required(CONTENT_S3_ACCESS_KEY_ID)?,
            required(CONTENT_S3_SECRET_ACCESS_KEY)?,
        );
        config.prefix = String::from(prefix);
        config.force_path_style = parse_bool(CONTENT_S3_FORCE_PATH_STYLE)?;

        Ok(Self { config })
    }

    /// Returns the content store configuration.
    pub(crate) fn config(&self) -> S3Config {
        self.config.clone()
    }
}

fn required(name: &str) -> Result<String, content_lifecycle::Error> {
    env_value(name).ok_or_else(|| {
        content_lifecycle::Error::Config(format!("{name} must be set and non-empty"))
    })
}

fn parse_bool(name: &str) -> Result<bool, content_lifecycle::Error> {
    let value = env_value(name);
    match value
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("" | "1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(content_lifecycle::Error::Config(format!(
            "{name} must be a boolean (got '{other}')"
        ))),
    }
}

fn env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn normalize_prefix(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_owned()
}
