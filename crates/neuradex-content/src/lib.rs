//! Content plane for neuradex: pack orchestration, object storage, and the
//! hourly request index.
//!
//! This crate owns *where* content lives. It does not depend on the HTTP
//! service or the audit event schema.
