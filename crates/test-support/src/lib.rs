//! Shared test harnesses for the neuradex workspace.
//!
//! The harnesses spin up isolated `httpmock` servers and drive the real clients and routers
//! over HTTP. They live outside the crates under test so several integration suites can share
//! the same high-level API without duplicating setup code or depending on each other's test
//! modules.
//!
//! The harnesses are feature-gated so a client-level test does not pull in the whole service:
//!
//! - `client`: `KagiHarness` for the `kagi` crate.
//! - `app`: `TestApp` for end-to-end tests of the `neuradex` HTTP service.

#[cfg(feature = "app")]
pub mod app;
#[cfg(feature = "client")]
pub mod kagi;
#[cfg(feature = "client")]
pub mod privacy_pass;

#[cfg(feature = "client")]
pub use httpmock;
