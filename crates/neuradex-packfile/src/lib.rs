//! Packfile format and reader/writer for neuradex content-addressed storage.
//!
//! This crate is deliberately free of async runtimes and object storage. It
//! owns the byte-level frame, footer, and trailer format.
