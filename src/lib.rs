//! A small API service implementing tools for LLM agents.
//!
//! Neuradex exposes a small set of HTTP endpoints under `/v1` that fetch web pages (with
//! browser-impersonating HTTP clients to defeat anti-bot systems) and search the web through
//! Kagi, returning metadata, bodies, and request metrics as JSON for consumption by LLM agents.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod http;
pub mod metadata;
pub mod metrics;
