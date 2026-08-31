//! Portable application contracts and the request admission gate.
//!
//! The request gate is deliberately the only place that can construct a
//! [`PreparedRequest`]. Keeping the fields private prevents an adapter or a
//! provider from accidentally bypassing validation.

pub mod cache;
pub mod config;
pub mod coordinator;
pub mod job;
pub mod normalize;
pub mod prompt;
pub mod request_gate;
pub mod response_format;
pub mod sentence;
pub mod text;

pub use config::{
    atomic_write, default_config_path, load_config, parse_toml, save_atomic, save_config_atomic,
    AppConfig, Config, ConfigError, DefaultProfiles, HotkeySettings, ProviderSettings, UiLanguage,
    UiSettings, ValidationError, DEFAULT_CREDENTIAL_TARGET, DEFAULT_CYCLE_PROFILES_HOTKEY,
};
pub use coordinator::{
    Completion, Coordinator, JobCancellation, JobHandle, JobPriority, JobStartRejection,
    TextFingerprint, AUTOMATIC_DUPLICATE_TTL,
};
pub use job::JobInput;
pub use prompt::{PromptConfig, PromptRenderError, PromptValidationError, RenderedPrompt};
pub use request_gate::{
    prepare_request, PreparedRequest, ProviderConfig, RequestGate, RequestRejection,
};
pub use response_format::normalize_terminal_response;
pub use text::{ExtractionSource, ScreenRect, TextContext, TriggerKind};

/// Marker retained for compatibility with the bootstrap crate.
pub const CRATE_NAME: &str = "selection-core";
