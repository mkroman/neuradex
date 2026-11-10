//! Packing and retention policy.

use std::time::Duration;

use neuradex_packfile::PackWriter;

use crate::PolicyError;

/// Default target size of one pack: 128 MiB of plaintext.
pub const DEFAULT_PACK_TARGET_BYTES: u64 = 128 * 1024 * 1024;

/// Default maximum age of an open pack: 10 minutes.
pub const DEFAULT_MAX_PACK_AGE: Duration = Duration::from_mins(10);

/// Default maximum captured body size: 25 MiB.
pub const DEFAULT_MAX_CAPTURED_BODY_BYTES: u64 = 25 * 1024 * 1024;

/// Default threshold at or above which a body is stored as an individual
/// object instead of being packed: 30 MiB.
///
/// This is larger than [`DEFAULT_MAX_CAPTURED_BODY_BYTES`], so with the default
/// policy every captured body is packed.
pub const DEFAULT_LARGE_BODY_THRESHOLD_BYTES: u64 = 30 * 1024 * 1024;

/// Default zstd compression level for packs.
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

/// Default retention period in days.
pub const DEFAULT_RETENTION_DAYS: u32 = 365;

/// Policy controlling pack size, pack age, and individual-object storage.
///
/// Pack targets are measured in appended plaintext bytes, not compressed file
/// size. The writer seals a pack once the target is reached, so a pack can
/// exceed the target by at most one body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasPolicy {
    pack_target_bytes: u64,
    max_pack_age: Duration,
    max_captured_body_bytes: u64,
    large_body_threshold_bytes: u64,
    compression_level: i32,
    retention_days: u32,
}

impl CasPolicy {
    /// Builds a validated policy.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] when a byte or duration limit is zero, or when
    /// the compression level is outside the range accepted by zstd.
    pub fn new(
        pack_target_bytes: u64,
        max_pack_age: Duration,
        max_captured_body_bytes: u64,
        large_body_threshold_bytes: u64,
        compression_level: i32,
        retention_days: u32,
    ) -> Result<Self, PolicyError> {
        if pack_target_bytes == 0 {
            return Err(PolicyError::ZeroPackTarget);
        }
        if max_pack_age.is_zero() {
            return Err(PolicyError::ZeroMaxPackAge);
        }
        if max_captured_body_bytes == 0 {
            return Err(PolicyError::ZeroMaxCapturedBody);
        }
        if large_body_threshold_bytes == 0 {
            return Err(PolicyError::ZeroLargeBodyThreshold);
        }
        if PackWriter::with_compression_level(std::io::sink(), compression_level).is_err() {
            return Err(PolicyError::InvalidCompressionLevel {
                level: compression_level,
            });
        }

        Ok(Self {
            pack_target_bytes,
            max_pack_age,
            max_captured_body_bytes,
            large_body_threshold_bytes,
            compression_level,
            retention_days,
        })
    }

    /// Returns the target number of plaintext bytes in one pack.
    #[must_use]
    pub const fn pack_target_bytes(&self) -> u64 {
        self.pack_target_bytes
    }

    /// Returns the maximum time an open pack may go unsealed.
    #[must_use]
    pub const fn max_pack_age(&self) -> Duration {
        self.max_pack_age
    }

    /// Returns the maximum body size the content plane accepts.
    #[must_use]
    pub const fn max_captured_body_bytes(&self) -> u64 {
        self.max_captured_body_bytes
    }

    /// Returns the size at or above which a body is stored as an object.
    #[must_use]
    pub const fn large_body_threshold_bytes(&self) -> u64 {
        self.large_body_threshold_bytes
    }

    /// Returns the zstd compression level used for packs.
    #[must_use]
    pub const fn compression_level(&self) -> i32 {
        self.compression_level
    }

    /// Returns the retention period in days.
    #[must_use]
    pub const fn retention_days(&self) -> u32 {
        self.retention_days
    }

    /// Returns whether a pack holding `bytes` plaintext bytes and open for
    /// `age` should be sealed.
    ///
    /// An empty pack is never considered due.
    #[must_use]
    pub fn should_flush(&self, bytes: u64, age: Duration) -> bool {
        bytes > 0 && (bytes >= self.pack_target_bytes || age >= self.max_pack_age)
    }

    /// Returns whether a body of `size` bytes should be stored as an individual
    /// object instead of joining a pack.
    #[must_use]
    pub const fn should_store_individually(&self, size: u64) -> bool {
        size >= self.large_body_threshold_bytes
    }
}

impl Default for CasPolicy {
    fn default() -> Self {
        Self {
            pack_target_bytes: DEFAULT_PACK_TARGET_BYTES,
            max_pack_age: DEFAULT_MAX_PACK_AGE,
            max_captured_body_bytes: DEFAULT_MAX_CAPTURED_BODY_BYTES,
            large_body_threshold_bytes: DEFAULT_LARGE_BODY_THRESHOLD_BYTES,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CasPolicy, DEFAULT_COMPRESSION_LEVEL, DEFAULT_LARGE_BODY_THRESHOLD_BYTES,
        DEFAULT_MAX_CAPTURED_BODY_BYTES, DEFAULT_MAX_PACK_AGE, DEFAULT_PACK_TARGET_BYTES,
        DEFAULT_RETENTION_DAYS,
    };
    use crate::PolicyError;

    #[test]
    fn defaults_match_the_documented_limits() {
        let policy = CasPolicy::default();

        assert_eq!(policy.pack_target_bytes(), 128 * 1024 * 1024);
        assert_eq!(policy.max_pack_age(), Duration::from_secs(600));
        assert_eq!(policy.max_captured_body_bytes(), 25 * 1024 * 1024);
        assert_eq!(policy.large_body_threshold_bytes(), 30 * 1024 * 1024);
        assert_eq!(policy.compression_level(), 3);
        assert_eq!(policy.retention_days(), 365);

        assert_eq!(policy.pack_target_bytes(), DEFAULT_PACK_TARGET_BYTES);
        assert_eq!(policy.max_pack_age(), DEFAULT_MAX_PACK_AGE);
        assert_eq!(
            policy.max_captured_body_bytes(),
            DEFAULT_MAX_CAPTURED_BODY_BYTES
        );
        assert_eq!(
            policy.large_body_threshold_bytes(),
            DEFAULT_LARGE_BODY_THRESHOLD_BYTES
        );
        assert_eq!(policy.compression_level(), DEFAULT_COMPRESSION_LEVEL);
        assert_eq!(policy.retention_days(), DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn flush_is_driven_by_size_or_age() {
        let policy = CasPolicy::default();

        assert!(!policy.should_flush(0, Duration::from_secs(3_600)));
        assert!(!policy.should_flush(1024, Duration::from_secs(1)));
        assert!(policy.should_flush(DEFAULT_PACK_TARGET_BYTES, Duration::ZERO));
        assert!(policy.should_flush(1024, DEFAULT_MAX_PACK_AGE));
    }

    #[test]
    fn large_bodies_are_stored_individually_at_the_threshold() {
        let policy = CasPolicy::default();

        assert!(!policy.should_store_individually(0));
        assert!(!policy.should_store_individually(DEFAULT_LARGE_BODY_THRESHOLD_BYTES - 1));
        assert!(policy.should_store_individually(DEFAULT_LARGE_BODY_THRESHOLD_BYTES));
    }

    #[test]
    fn zero_limits_are_rejected() {
        assert!(matches!(
            CasPolicy::new(0, Duration::from_secs(1), 1, 1, 3, 1),
            Err(PolicyError::ZeroPackTarget)
        ));
        assert!(matches!(
            CasPolicy::new(1, Duration::ZERO, 1, 1, 3, 1),
            Err(PolicyError::ZeroMaxPackAge)
        ));
        assert!(matches!(
            CasPolicy::new(1, Duration::from_secs(1), 0, 1, 3, 1),
            Err(PolicyError::ZeroMaxCapturedBody)
        ));
        assert!(matches!(
            CasPolicy::new(1, Duration::from_secs(1), 1, 0, 3, 1),
            Err(PolicyError::ZeroLargeBodyThreshold)
        ));
    }

    #[test]
    fn compression_level_is_validated() {
        assert!(matches!(
            CasPolicy::new(1, Duration::from_secs(1), 1, 1, i32::MAX, 1),
            Err(PolicyError::InvalidCompressionLevel { level: i32::MAX })
        ));
        assert!(
            CasPolicy::new(1, Duration::from_secs(1), 1, 1, 3, 1).is_ok(),
            "the default level is accepted"
        );
    }
}
