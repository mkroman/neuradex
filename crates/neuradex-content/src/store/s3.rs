//! S3-compatible content store.
//!
//! This backend uses the official [`aws-sdk-s3`] crate and targets both AWS S3
//! and S3-compatible services such as Backblaze B2. B2 does not implement every
//! AWS extension, so the store deliberately:
//!
//! - uses an explicit endpoint, static credentials, and path-style addressing;
//! - never uses object tagging, bucket lifecycle configuration, or IAM roles;
//! - sets `request_checksum_calculation` and `response_checksum_validation` to
//!   `WhenRequired`, which keeps the SDK from adding `x-amz-checksum-*` headers
//!   that B2 rejects;
//! - uses multipart upload for large packs instead of buffering a whole pack,
//!   and aborts the upload on any error.
//!
//! Credentials are supplied explicitly because instance metadata and IAM role
//! discovery are AWS-specific and do not exist on B2.

use std::fmt;
use std::io::{BufReader, Read};
use std::ops::Range;
use std::path::Path;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};

use super::{validate_key, with_prefix};
use crate::StoreError;

/// Default size of one multipart upload part: 16 MiB.
pub const DEFAULT_MULTIPART_PART_SIZE: u64 = 16 * 1024 * 1024;

/// Default maximum bytes returned by one range read: 256 MiB.
pub const DEFAULT_MAX_RANGE_BYTES: u64 = 256 * 1024 * 1024;

/// S3's smallest allowed multipart part size (except for the final part).
const MIN_MULTIPART_PART_SIZE: u64 = 5 * 1024 * 1024;

/// S3's largest allowed multipart part size.
const MAX_MULTIPART_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// S3's maximum number of parts in one multipart upload.
const MAX_MULTIPART_PARTS: u64 = 10_000;

/// Configuration for [`S3Store`].
#[derive(Clone)]
pub struct S3Config {
    /// Endpoint URL, for example `https://s3.us-west-004.backblazeb2.com`.
    pub endpoint_url: String,
    /// Signing region, for example `us-west-004`.
    pub region: String,
    /// Bucket name.
    pub bucket: String,
    /// Optional key prefix, without a leading or trailing slash.
    pub prefix: String,
    /// Access key identifier.
    pub access_key_id: String,
    /// Secret access key. It is redacted from [`Debug`] output.
    pub secret_access_key: String,
    /// Whether to use path-style addressing. B2 requires this.
    pub force_path_style: bool,
    /// Size of each multipart part, in bytes.
    pub multipart_part_size: u64,
    /// Maximum number of bytes returned by one range read.
    pub max_range_bytes: u64,
}

impl S3Config {
    /// Builds a B2-friendly configuration with path-style addressing and the
    /// default part and range limits.
    #[must_use]
    pub fn new(
        endpoint_url: impl Into<String>,
        region: impl Into<String>,
        bucket: impl Into<String>,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            region: region.into(),
            bucket: bucket.into(),
            prefix: String::new(),
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            force_path_style: true,
            multipart_part_size: DEFAULT_MULTIPART_PART_SIZE,
            max_range_bytes: DEFAULT_MAX_RANGE_BYTES,
        }
    }
}

impl fmt::Debug for S3Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3Config")
            .field("endpoint_url", &self.endpoint_url)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("force_path_style", &self.force_path_style)
            .field("multipart_part_size", &self.multipart_part_size)
            .field("max_range_bytes", &self.max_range_bytes)
            .finish()
    }
}

/// An S3-compatible content store.
#[derive(Clone)]
pub struct S3Store {
    client: Client,
    config: S3Config,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3Store")
            .field("endpoint_url", &self.config.endpoint_url)
            .field("bucket", &self.config.bucket)
            .field("prefix", &self.config.prefix)
            .finish_non_exhaustive()
    }
}

impl S3Store {
    /// Builds a client from `config`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidConfig`] when required strings are empty,
    /// the prefix is unsafe, or the part/range limits are outside S3's allowed
    /// bounds.
    pub fn new(mut config: S3Config) -> Result<Self, StoreError> {
        if config.endpoint_url.trim().is_empty() {
            return Err(StoreError::InvalidConfig {
                message: "endpoint URL must not be empty".to_owned(),
            });
        }
        if config.region.trim().is_empty() {
            return Err(StoreError::InvalidConfig {
                message: "region must not be empty".to_owned(),
            });
        }
        if config.bucket.trim().is_empty() {
            return Err(StoreError::InvalidConfig {
                message: "bucket must not be empty".to_owned(),
            });
        }
        if config.access_key_id.is_empty() || config.secret_access_key.is_empty() {
            return Err(StoreError::InvalidConfig {
                message: "access key and secret must not be empty".to_owned(),
            });
        }
        if !(MIN_MULTIPART_PART_SIZE..=MAX_MULTIPART_PART_SIZE)
            .contains(&config.multipart_part_size)
        {
            return Err(StoreError::InvalidConfig {
                message: format!(
                    "multipart part size must be between {MIN_MULTIPART_PART_SIZE} and \
                     {MAX_MULTIPART_PART_SIZE} bytes"
                ),
            });
        }
        if config.max_range_bytes == 0 {
            return Err(StoreError::InvalidConfig {
                message: "max range bytes must be greater than zero".to_owned(),
            });
        }

        config.prefix = config.prefix.trim_end_matches('/').to_owned();
        if !config.prefix.is_empty() {
            validate_key(&config.prefix)?;
        }

        let credentials = Credentials::new(
            config.access_key_id.clone(),
            config.secret_access_key.clone(),
            None,
            None,
            "neuradex-content",
        );
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .endpoint_url(config.endpoint_url.clone())
            .credentials_provider(credentials)
            .force_path_style(config.force_path_style)
            // B2 rejects or mishandles the SDK's default automatic checksum
            // headers. `WhenRequired` keeps them off operations that do not
            // require a checksum.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build();

        Ok(Self {
            client: Client::from_conf(sdk_config),
            config,
        })
    }

    /// Writes `bytes` to `key` with a single `PutObject` call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or a
    /// [`StoreError::S3`] error when the request fails.
    pub async fn put_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let key = self.full_key(key)?;
        self.put_object_bytes(&key, bytes).await
    }

    /// Uploads the file at `path` to `key`.
    ///
    /// Files at or below the configured part size use one `PutObject` call.
    /// Larger files use a multipart upload so the whole pack is never held in
    /// memory. Any multipart failure is followed by a best-effort abort. File
    /// reads use `std::fs` and block the calling task.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, an I/O error when
    /// the file cannot be read, [`StoreError::MultipartTooLarge`] when the file
    /// exceeds the part-count limit, or a [`StoreError::S3`] error when an S3
    /// request fails.
    pub async fn put_file(&self, key: &str, path: &Path) -> Result<(), StoreError> {
        let key = self.full_key(key)?;
        let size = std::fs::metadata(path)?.len();
        // Never buffer a whole pack just because the configured part size is
        // large: small uploads stay bounded by the default part size.
        let single_put_limit = self
            .config
            .multipart_part_size
            .min(DEFAULT_MULTIPART_PART_SIZE);

        if size <= single_put_limit {
            let bytes = std::fs::read(path)?;
            return self.put_object_bytes(&key, &bytes).await;
        }
        self.put_file_multipart(&key, path, size).await
    }

    /// Reads `range` (exclusive end) from `key`.
    ///
    /// The requested length must not exceed the configured
    /// [`max_range_bytes`](S3Config::max_range_bytes).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidKey`] for an unsafe key,
    /// [`StoreError::NotFound`] when the key does not exist,
    /// [`StoreError::RangeTooLarge`] when the request exceeds the configured
    /// cap, or a [`StoreError::S3`] error when the request fails.
    pub async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, StoreError> {
        let key = self.full_key(key)?;
        if range.end <= range.start {
            return Ok(Vec::new());
        }

        let length =
            range
                .end
                .checked_sub(range.start)
                .ok_or_else(|| StoreError::InvalidConfig {
                    message: "invalid range".to_owned(),
                })?;
        if length > self.config.max_range_bytes {
            return Err(StoreError::RangeTooLarge {
                start: range.start,
                end: range.end,
                limit: self.config.max_range_bytes,
            });
        }

        let output = self
            .client
            .get_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .range(format!("bytes={}-{}", range.start, range.end - 1))
            .send()
            .await
            .map_err(|error| match error.as_service_error() {
                Some(service) if service.is_no_such_key() => {
                    StoreError::NotFound { key: key.clone() }
                }
                _ => s3_error(error),
            })?;

        if let Some(content_length) = output.content_length()
            && u64::try_from(content_length).map_or(true, |content_length| content_length > length)
        {
            return Err(StoreError::OversizedResponse { key, limit: length });
        }

        let expected = usize::try_from(length).map_err(|_| StoreError::InvalidConfig {
            message: "requested range does not fit in memory".to_owned(),
        })?;
        let mut body = output.body;
        let mut bytes = Vec::with_capacity(expected);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(boxed_store_error)?;
            if bytes.len().saturating_add(chunk.len()) > expected {
                return Err(StoreError::OversizedResponse { key, limit: length });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// Returns the object length in bytes using one `HeadObject` request.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the key does not exist, or a
    /// [`StoreError::S3`] error when the request fails.
    pub async fn object_size(&self, key: &str) -> Result<u64, StoreError> {
        let key = self.full_key(key)?;
        let output = self
            .client
            .head_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|error| match error.as_service_error() {
                Some(service) if service.is_not_found() => {
                    StoreError::NotFound { key: key.clone() }
                }
                _ => s3_error(error),
            })?;

        let content_length = output
            .content_length()
            .ok_or(StoreError::MissingResponseField {
                field: "Content-Length",
            })?;
        u64::try_from(content_length).map_err(|_| StoreError::MissingResponseField {
            field: "Content-Length",
        })
    }

    /// Returns whether `key` exists using one `HeadObject` request.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError::S3`] error other than "not found" when the
    /// request fails.
    pub async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let key = self.full_key(key)?;
        match self
            .client
            .head_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_s3::operation::head_object::HeadObjectError::is_not_found,
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(s3_error(error)),
        }
    }

    fn full_key(&self, key: &str) -> Result<String, StoreError> {
        validate_key(key)?;
        let full = with_prefix(&self.config.prefix, key);
        if full.len() > 1024 {
            return Err(StoreError::InvalidKey {
                key: key.to_owned(),
            });
        }
        Ok(full)
    }

    async fn put_object_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.client
            .put_object()
            .bucket(&self.config.bucket)
            .key(key)
            .body(ByteStream::from(bytes.to_vec()))
            .send()
            .await
            .map_err(s3_error)?;
        Ok(())
    }

    async fn put_file_multipart(
        &self,
        key: &str,
        path: &Path,
        size: u64,
    ) -> Result<(), StoreError> {
        let max_size = self
            .config
            .multipart_part_size
            .checked_mul(MAX_MULTIPART_PARTS)
            .ok_or_else(|| StoreError::InvalidConfig {
                message: "multipart limits overflowed".to_owned(),
            })?;
        if size > max_size {
            return Err(StoreError::MultipartTooLarge {
                size,
                limit: max_size,
            });
        }

        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)?;
        let upload_id = created
            .upload_id()
            .ok_or(StoreError::MissingResponseField { field: "UploadId" })?
            .to_owned();

        let parts = match self.upload_parts(key, path, &upload_id).await {
            Ok(parts) => parts,
            Err(error) => {
                self.abort_multipart(key, &upload_id).await;
                return Err(error);
            }
        };

        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        if let Err(error) = self
            .client
            .complete_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
        {
            self.abort_multipart(key, &upload_id).await;
            return Err(s3_error(error));
        }
        Ok(())
    }

    async fn upload_parts(
        &self,
        key: &str,
        path: &Path,
        upload_id: &str,
    ) -> Result<Vec<CompletedPart>, StoreError> {
        let part_size = usize::try_from(self.config.multipart_part_size).map_err(|_| {
            StoreError::InvalidConfig {
                message: "multipart part size does not fit in memory".to_owned(),
            }
        })?;
        let mut reader = BufReader::new(std::fs::File::open(path)?);
        let mut parts = Vec::new();
        let mut part_number = 1_i32;

        loop {
            let mut buffer = vec![0_u8; part_size];
            let mut read = 0;
            while read < part_size {
                let count = reader.read(&mut buffer[read..])?;
                if count == 0 {
                    break;
                }
                read += count;
            }
            if read == 0 {
                break;
            }
            buffer.truncate(read);

            let output = self
                .client
                .upload_part()
                .bucket(&self.config.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(buffer))
                .send()
                .await
                .map_err(s3_error)?;
            let e_tag = output
                .e_tag()
                .ok_or(StoreError::MissingResponseField { field: "ETag" })?
                .to_owned();
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(e_tag)
                    .build(),
            );

            part_number = part_number
                .checked_add(1)
                .ok_or(StoreError::MultipartTooLarge {
                    size: u64::MAX,
                    limit: self
                        .config
                        .multipart_part_size
                        .saturating_mul(MAX_MULTIPART_PARTS),
                })?;
        }

        Ok(parts)
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await;
    }
}

fn s3_error<E>(error: SdkError<E>) -> StoreError
where
    E: std::error::Error + Send + Sync + 'static,
{
    boxed_store_error(error)
}

fn boxed_store_error<E>(error: E) -> StoreError
where
    E: std::error::Error + Send + Sync + 'static,
{
    StoreError::S3(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_RANGE_BYTES, DEFAULT_MULTIPART_PART_SIZE, S3Config, S3Store};

    fn config() -> S3Config {
        S3Config::new(
            "https://s3.example.test",
            "us-west-004",
            "bucket",
            "key-id",
            "very-secret-value",
        )
    }

    #[test]
    fn defaults_are_b2_friendly() {
        let config = config();

        assert!(config.force_path_style);
        assert_eq!(config.multipart_part_size, DEFAULT_MULTIPART_PART_SIZE);
        assert_eq!(config.max_range_bytes, DEFAULT_MAX_RANGE_BYTES);
        assert_eq!(config.prefix, "");
    }

    #[test]
    fn debug_redacts_the_secret() {
        let rendered = format!("{:?}", config());
        assert!(!rendered.contains("very-secret-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn prefix_is_normalized_and_applied_once() {
        let mut configured = config();
        configured.prefix = "tenant/".to_owned();
        let store = S3Store::new(configured).unwrap();

        assert_eq!(store.full_key("objects/abc").unwrap(), "tenant/objects/abc");
        assert!(store.full_key("../escape").is_err());
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        assert!(S3Store::new(config()).is_ok());

        let mut empty_endpoint = config();
        empty_endpoint.endpoint_url.clear();
        assert!(S3Store::new(empty_endpoint).is_err());

        let mut small_parts = config();
        small_parts.multipart_part_size = 1;
        assert!(S3Store::new(small_parts).is_err());

        let mut bad_prefix = config();
        bad_prefix.prefix = "../escape".to_owned();
        assert!(S3Store::new(bad_prefix).is_err());

        let mut empty_range = config();
        empty_range.max_range_bytes = 0;
        assert!(S3Store::new(empty_range).is_err());
    }
}
