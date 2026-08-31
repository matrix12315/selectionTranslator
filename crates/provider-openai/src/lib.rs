//! OpenAI-compatible provider integration.
//!
//! The public entry point accepts only the request-gate's `PreparedRequest`.
//! Transport is synchronous at the API boundary but always performs network
//! work on a dedicated worker thread; no async runtime is used.

mod client;
mod error;
mod sse;

pub use client::{CancellationToken, DeltaSink, OpenAiConfig, OpenAiProvider};
pub use error::{ProviderError, ProviderResult};

/// Marker retained for compatibility with the bootstrap crate.
pub const CRATE_NAME: &str = "selection-provider-openai";
