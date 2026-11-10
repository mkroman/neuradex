//! Content retention lifecycle commands.

use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, BucketVersioningStatus, ExpirationStatus, LifecycleExpiration,
    LifecycleRule, LifecycleRuleFilter, NoncurrentVersionExpiration,
};
use clap::Subcommand;
use neuradex_content::S3Store;
use serde::Serialize;

use crate::config::{PolicySettings, S3Settings};

/// The operations exposed by `neuradex-admin content-lifecycle`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the generated policy as JSON. Does not call S3.
    Print,
    /// Compare the bucket policy with the generated policy; exit 1 on drift.
    Check,
    /// Replace the bucket lifecycle configuration with the generated policy.
    Apply,
}

/// Errors from the lifecycle commands.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required setting is missing or invalid.
    #[error("configuration error: {0}")]
    Config(String),
    /// The content store configuration is invalid.
    #[error("content store error: {0}")]
    Store(#[from] neuradex_content::StoreError),
    /// An S3 operation failed.
    #[error("S3 request failed: {0}")]
    S3(String),
    /// A generated policy could not be built.
    #[error("lifecycle policy is invalid: {0}")]
    Policy(String),
    /// The bucket is not versioning-enabled.
    #[error(
        "bucket {bucket} does not have versioning enabled (status: {status}); B2 lifecycle rules \
         require a versioning-enabled bucket"
    )]
    Versioning {
        /// The bucket name.
        bucket: String,
        /// The observed status.
        status: String,
    },
    /// The bucket has no lifecycle configuration.
    #[error(
        "bucket {bucket} has no lifecycle configuration; review `content-lifecycle print` and \
         run `content-lifecycle apply`"
    )]
    NoLifecycle {
        /// The bucket name.
        bucket: String,
    },
    /// The bucket configuration differs from the generated policy.
    #[error("lifecycle configuration drift on bucket {bucket}")]
    Drift {
        /// The bucket name.
        bucket: String,
    },
}

/// Runs a lifecycle command.
pub async fn run(command: Command) -> Result<(), Error> {
    match command {
        Command::Print => {
            let settings = PolicySettings::from_env()?;
            let policy = Policy::new(&settings);
            println!(
                "{}",
                serde_json::to_string_pretty(&policy)
                    .map_err(|error| Error::Policy(error.to_string()))?
            );
            Ok(())
        }
        Command::Check | Command::Apply => {
            let settings = PolicySettings::from_env()?;
            let s3 = S3Settings::from_env(settings.prefix())?;
            run_s3(command, &settings, s3).await
        }
    }
}

async fn run_s3(command: Command, settings: &PolicySettings, s3: S3Settings) -> Result<(), Error> {
    let store = S3Store::new(s3.config())?;
    let client = store.client();
    let bucket = store.bucket();
    let policy = Policy::new(settings);

    ensure_versioning(client, bucket).await?;

    match command {
        Command::Print => unreachable!("print is handled without S3"),
        Command::Check => check_policy(client, bucket, &policy).await,
        Command::Apply => apply_policy(client, bucket, &policy).await,
    }
}

async fn ensure_versioning(client: &Client, bucket: &str) -> Result<(), Error> {
    let output = client
        .get_bucket_versioning()
        .bucket(bucket)
        .send()
        .await
        .map_err(s3_error)?;
    if matches!(output.status(), Some(BucketVersioningStatus::Enabled)) {
        return Ok(());
    }

    Err(Error::Versioning {
        bucket: bucket.to_owned(),
        status: output.status().map_or_else(
            || "disabled".to_owned(),
            |status| status.as_str().to_owned(),
        ),
    })
}

async fn check_policy(client: &Client, bucket: &str, policy: &Policy) -> Result<(), Error> {
    let output = match client
        .get_bucket_lifecycle_configuration()
        .bucket(bucket)
        .send()
        .await
    {
        Ok(output) => output,
        Err(error)
            if error
                .as_service_error()
                .and_then(ProvideErrorMetadata::code)
                == Some("NoSuchLifecycleConfiguration") =>
        {
            return Err(Error::NoLifecycle {
                bucket: bucket.to_owned(),
            });
        }
        Err(error) => return Err(s3_error(error)),
    };

    let mut remote = output
        .rules()
        .iter()
        .map(CanonicalRule::from_sdk)
        .collect::<Vec<_>>();
    let mut desired = policy.canonical_rules();
    remote.sort();
    desired.sort();

    if remote == desired {
        println!(
            "bucket {bucket} lifecycle configuration matches the generated policy ({} rules)",
            desired.len()
        );
        return Ok(());
    }

    eprintln!("lifecycle configuration drift on bucket {bucket}:");
    for rule in &remote {
        eprintln!("  remote: {rule:?}");
    }
    for rule in &desired {
        eprintln!("  desired: {rule:?}");
    }
    Err(Error::Drift {
        bucket: bucket.to_owned(),
    })
}

async fn apply_policy(client: &Client, bucket: &str, policy: &Policy) -> Result<(), Error> {
    let rules = policy.sdk_rules()?;
    let rule_count = rules.len();
    let configuration = BucketLifecycleConfiguration::builder()
        .set_rules(Some(rules))
        .build()
        .map_err(|error| Error::Policy(error.to_string()))?;

    client
        .put_bucket_lifecycle_configuration()
        .bucket(bucket)
        .lifecycle_configuration(configuration)
        .send()
        .await
        .map_err(s3_error)?;

    println!(
        "applied {rule_count}-rule lifecycle policy to bucket {bucket} (retention {} days)",
        policy.retention_days
    );
    Ok(())
}

fn s3_error(error: impl std::fmt::Display) -> Error {
    Error::S3(error.to_string())
}

/// The JSON lifecycle policy written to stdout.
#[derive(Debug, Serialize)]
struct Policy {
    #[serde(rename = "Rules")]
    rules: Vec<PolicyRule>,
    #[serde(skip)]
    retention_days: u32,
}

impl Policy {
    fn new(settings: &PolicySettings) -> Self {
        let mut rules = Vec::new();
        for name in ["packs", "objects", "index", "events"] {
            let prefix = if settings.prefix().is_empty() {
                format!("{name}/")
            } else {
                format!("{}/{name}/", settings.prefix())
            };
            rules.push(PolicyRule {
                id: format!("neuradex-{name}-current"),
                status: "Enabled",
                filter: Filter {
                    prefix: prefix.clone(),
                },
                expiration: Some(Expiration {
                    days: Some(settings.retention_days()),
                    expired_object_delete_marker: None,
                }),
                noncurrent: None,
            });
            rules.push(PolicyRule {
                id: format!("neuradex-{name}-delete-markers"),
                status: "Enabled",
                filter: Filter {
                    prefix: prefix.clone(),
                },
                expiration: Some(Expiration {
                    days: None,
                    expired_object_delete_marker: Some(true),
                }),
                noncurrent: None,
            });
            rules.push(PolicyRule {
                id: format!("neuradex-{name}-noncurrent"),
                status: "Enabled",
                filter: Filter { prefix },
                expiration: None,
                noncurrent: Some(Noncurrent { noncurrent_days: 1 }),
            });
        }

        Self {
            rules,
            retention_days: settings.retention_days(),
        }
    }

    fn sdk_rules(&self) -> Result<Vec<LifecycleRule>, Error> {
        self.rules
            .iter()
            .map(|rule| {
                let mut builder = LifecycleRule::builder()
                    .id(rule.id.clone())
                    .filter(
                        LifecycleRuleFilter::builder()
                            .prefix(rule.filter.prefix.clone())
                            .build(),
                    )
                    .status(ExpirationStatus::Enabled);

                if let Some(expiration) = &rule.expiration {
                    let mut expiration_builder = LifecycleExpiration::builder();
                    if let Some(days) = expiration.days {
                        expiration_builder = expiration_builder.days(
                            i32::try_from(days)
                                .map_err(|_| Error::Policy("retention days exceed i32".into()))?,
                        );
                    }
                    if let Some(marker) = expiration.expired_object_delete_marker {
                        expiration_builder =
                            expiration_builder.expired_object_delete_marker(marker);
                    }
                    builder = builder.expiration(expiration_builder.build());
                }

                if let Some(noncurrent) = &rule.noncurrent {
                    builder = builder.noncurrent_version_expiration(
                        NoncurrentVersionExpiration::builder()
                            .noncurrent_days(noncurrent.noncurrent_days)
                            .build(),
                    );
                }

                builder
                    .build()
                    .map_err(|error| Error::Policy(error.to_string()))
            })
            .collect()
    }

    fn canonical_rules(&self) -> Vec<CanonicalRule> {
        self.rules
            .iter()
            .map(|rule| CanonicalRule {
                prefix: rule.filter.prefix.clone(),
                days: rule
                    .expiration
                    .as_ref()
                    .and_then(|expiration| expiration.days),
                delete_marker: rule
                    .expiration
                    .as_ref()
                    .and_then(|expiration| expiration.expired_object_delete_marker),
                noncurrent_days: rule
                    .noncurrent
                    .as_ref()
                    .map(|noncurrent| noncurrent.noncurrent_days),
                enabled: true,
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct PolicyRule {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Filter")]
    filter: Filter,
    #[serde(rename = "Status")]
    status: &'static str,
    #[serde(rename = "Expiration", skip_serializing_if = "Option::is_none")]
    expiration: Option<Expiration>,
    #[serde(
        rename = "NoncurrentVersionExpiration",
        skip_serializing_if = "Option::is_none"
    )]
    noncurrent: Option<Noncurrent>,
}

#[derive(Debug, Serialize)]
struct Filter {
    #[serde(rename = "Prefix")]
    prefix: String,
}

#[derive(Debug, Serialize)]
struct Expiration {
    #[serde(rename = "Days", skip_serializing_if = "Option::is_none")]
    days: Option<u32>,
    #[serde(
        rename = "ExpiredObjectDeleteMarker",
        skip_serializing_if = "Option::is_none"
    )]
    expired_object_delete_marker: Option<bool>,
}

#[derive(Debug, Serialize)]
struct Noncurrent {
    #[serde(rename = "NoncurrentDays")]
    noncurrent_days: i32,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalRule {
    prefix: String,
    days: Option<u32>,
    delete_marker: Option<bool>,
    noncurrent_days: Option<i32>,
    enabled: bool,
}

impl CanonicalRule {
    fn from_sdk(rule: &LifecycleRule) -> Self {
        // Keep the fields in one place so the generated and remote policies use the same
        // normalization. Rule IDs are intentionally ignored: B2 regenerates them.
        let expiration = rule.expiration();
        Self {
            prefix: rule
                .filter()
                .and_then(LifecycleRuleFilter::prefix)
                .unwrap_or_default()
                .to_owned(),
            days: expiration
                .and_then(LifecycleExpiration::days)
                .and_then(|days| u32::try_from(days).ok()),
            delete_marker: expiration.and_then(LifecycleExpiration::expired_object_delete_marker),
            noncurrent_days: rule
                .noncurrent_version_expiration()
                .and_then(NoncurrentVersionExpiration::noncurrent_days),
            enabled: matches!(rule.status(), ExpirationStatus::Enabled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(retention_days: u32, prefix: &str) -> PolicySettings {
        PolicySettings::new(retention_days, prefix)
    }

    #[test]
    fn generates_the_three_rule_pairs_for_every_prefix() {
        let policy = Policy::new(&settings(365, ""));
        assert_eq!(policy.rules.len(), 12);
        assert_eq!(
            policy
                .rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            12
        );
        for prefix in ["packs/", "objects/", "index/", "events/"] {
            assert_eq!(
                policy
                    .rules
                    .iter()
                    .filter(|rule| rule.filter.prefix == prefix)
                    .count(),
                3
            );
        }
    }

    #[test]
    fn nests_the_policy_under_the_content_prefix() {
        let policy = Policy::new(&settings(90, "tenant"));
        assert!(policy.rules.iter().all(|rule| {
            rule.filter.prefix.starts_with("tenant/")
                && ["packs/", "objects/", "index/", "events/"]
                    .iter()
                    .any(|name| rule.filter.prefix.ends_with(name))
        }));
    }

    #[test]
    fn serializes_to_the_s3_lifecycle_shape() {
        let policy = Policy::new(&settings(365, ""));
        let value = serde_json::to_value(&policy).expect("the policy serializes");
        assert_eq!(value["Rules"].as_array().expect("rules").len(), 12);
        assert_eq!(value["Rules"][0]["Status"], "Enabled");
        assert_eq!(value["Rules"][0]["Filter"]["Prefix"], "packs/");
        assert_eq!(value["Rules"][0]["Expiration"]["Days"], 365);
    }

    #[test]
    fn builds_sdk_rules_without_a_network() {
        let policy = Policy::new(&settings(365, "tenant"));
        let rules = policy.sdk_rules().expect("the SDK rules build");
        assert_eq!(rules.len(), 12);
        assert!(rules.iter().all(|rule| {
            rule.status() == &ExpirationStatus::Enabled
                && rule
                    .filter()
                    .and_then(LifecycleRuleFilter::prefix)
                    .is_some_and(|prefix| prefix.starts_with("tenant/"))
        }));
    }

    #[test]
    fn canonical_rules_ignore_rule_ids() {
        let first = Policy::new(&settings(365, ""));
        let second = Policy::new(&settings(365, ""));
        assert_eq!(first.canonical_rules(), second.canonical_rules());
    }
}
