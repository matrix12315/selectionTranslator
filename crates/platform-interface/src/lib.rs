//! Portable interfaces implemented by platform adapters.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub use selection_core::{
    ExtractionSource, JobInput, PreparedRequest, ScreenRect, TextContext, TriggerKind,
};

/// A pointer coordinate in virtual-screen coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub struct ScreenPoint {
    pub x: i32,
    pub y: i32,
}

impl ScreenPoint {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractionFailure {
    UnsupportedPattern,
    EmptyRange,
    PermissionDenied,
    StaleElement,
    Platform,
}

impl ExtractionFailure {
    #[allow(non_upper_case_globals)]
    pub const Unsupported: Self = Self::UnsupportedPattern;
    #[allow(non_upper_case_globals)]
    pub const Empty: Self = Self::EmptyRange;
}

/// The extraction result is a standard result so adapters can use `?` while
/// preserving structured local failure reasons.
pub type ExtractionResult = Result<TextContext, ExtractionFailure>;

pub trait TextExtractor {
    fn extract(
        &self,
        trigger: TriggerKind,
        pointer: Option<ScreenPoint>,
        selection_rect: Option<ScreenRect>,
    ) -> ExtractionResult;
}

/// Cancellation is shared by extraction and provider workers without an
/// async runtime. Cloning a token creates another handle to the same flag.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A provider failure category safe to carry across the platform boundary.
///
/// Variants intentionally contain no endpoint, response body, prompt, target,
/// credential, or raw operating-system error.  The resident maps these
/// categories to short local messages before handing them to the popup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    Cancelled,
    Configuration,
    Authentication,
    HttpStatus(u16),
    RateLimited,
    Dns,
    Tls,
    Timeout,
    Transport,
    MalformedResponse,
    IncompleteResponse,
    ResponseTooLarge,
    Unavailable,
    /// Compatibility category for platform adapters that cannot classify a
    /// provider failure more precisely. Its payload must be sanitized before
    /// reaching a user-facing surface.
    Local(String),
    InvalidResponse,
}

pub type ProviderResult = Result<(), ProviderError>;

pub trait TranslationProvider {
    fn stream(
        &self,
        prepared: &PreparedRequest,
        cancellation: &CancellationToken,
        sink: &mut dyn PopupSink,
    ) -> ProviderResult;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedEntry {
    pub created_at_utc: String,
    pub source: ExtractionSource,
    pub target: String,
    pub context: Option<String>,
    pub output: String,
    pub prompt_id: String,
    pub model: String,
    pub served_from_cache: bool,
}

/// Return the canonical UTC timestamp used by history rows.  This stays in
/// the portable interface so every platform records the same sortable shape
/// without pulling a date/time dependency into the resident.
pub fn canonical_utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_date_from_days(days as i64);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// Howard Hinnant's proleptic-Gregorian conversion, expressed without a date
// dependency. Unix epoch day 0 is 1970-01-01.
fn civil_date_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryError(pub String);

/// Receives completed entries. Implementations may enqueue work so callers
/// on the resident message-loop thread never perform blocking persistence.
pub trait HistoryStore: Send + Sync {
    fn insert_completed(&self, entry: CompletedEntry) -> Result<(), HistoryError>;
}

pub trait PopupSink {
    fn show_loading(&mut self, job_id: u64);
    fn update(&mut self, job_id: u64, delta: &str);
    fn finish(&mut self, job_id: u64);
    fn show_local_error(&mut self, job_id: u64, message: &str);
    fn dismiss(&mut self, job_id: u64);
}

/// Marker retained for compatibility with the bootstrap crate.
pub const CRATE_NAME: &str = "selection-platform-interface";

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use selection_core::{
        ExtractionSource, JobInput, PromptConfig, ProviderConfig, RequestGate, TextContext,
    };

    use super::{
        CancellationToken, PopupSink, PreparedRequest, ProviderResult, TranslationProvider,
    };

    struct FakeProvider {
        calls: AtomicUsize,
    }

    impl TranslationProvider for FakeProvider {
        fn stream(
            &self,
            _prepared: &PreparedRequest,
            _cancellation: &CancellationToken,
            _sink: &mut dyn PopupSink,
        ) -> ProviderResult {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct Sink;
    impl PopupSink for Sink {
        fn show_loading(&mut self, _: u64) {}
        fn update(&mut self, _: u64, _: &str) {}
        fn finish(&mut self, _: u64) {}
        fn show_local_error(&mut self, _: u64, _: &str) {}
        fn dismiss(&mut self, _: u64) {}
    }

    fn gate() -> RequestGate {
        RequestGate::new(
            ProviderConfig::new("https://example.invalid/v1", "model"),
            [PromptConfig::new("translate")],
        )
    }

    fn input(id: u64, target: &str) -> JobInput {
        JobInput::new(
            id,
            selection_core::TriggerKind::Manual,
            TextContext {
                target: target.to_owned(),
                context: Some("context".to_owned()),
                source: ExtractionSource::UiaSelection,
                screen_rect: None,
            },
            "translate",
        )
    }

    #[test]
    fn every_rejected_input_makes_zero_provider_calls() {
        let provider = FakeProvider {
            calls: AtomicUsize::new(0),
        };
        let token = CancellationToken::new();
        let mut sink = Sink;
        let cases = [
            (input(1, ""), false, 1),
            (input(1, "\u{200B}\u{2060}\u{FEFF}"), false, 1),
            (input(1, " \u{3000}\n"), false, 1),
            (input(1, &"x".repeat(4001)), false, 1),
            (input(1, "valid"), true, 1),
            (input(1, "valid"), false, 2),
        ];
        for (job, cancelled, active_id) in cases {
            assert!(gate().prepare(&job, active_id, cancelled).is_err());
        }
        assert_eq!(provider.calls.load(Ordering::Relaxed), 0);

        let prepared = gate().prepare(&input(1, "valid"), 1, false).unwrap();
        provider.stream(&prepared, &token, &mut sink).unwrap();
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn canonical_timestamp_is_utc_and_sortable() {
        let timestamp = super::canonical_utc_now();
        assert_eq!(timestamp.len(), 20);
        assert_eq!(timestamp.as_bytes()[10], b'T');
        assert_eq!(timestamp.as_bytes()[19], b'Z');
    }
}
