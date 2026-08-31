//! Resident-side composition workers.
//!
//! The message-loop thread owns coordination, admission, cache, and popup
//! state.  Extraction and provider work are dispatched to workers; workers
//! communicate back with small, owned messages and post one wake-up message
//! to the resident window.

use selection_core::{
    AppConfig, ExtractionSource, ProviderConfig, RequestGate, ScreenRect, TextContext, TriggerKind,
};
use selection_platform_interface::{
    CancellationToken, ExtractionFailure, ExtractionResult, HistoryStore, PopupSink, ProviderError,
    TranslationProvider,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use crate::clipboard::ClipboardExtractor;
use crate::native_selection;
use crate::ocr::OcrExtractor;
use crate::runtime_trace;
use crate::uia::UiaExtractor;

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// The app drains the event receiver after receiving this message.
#[cfg(windows)]
pub const PIPELINE_EVENT: u32 = WM_APP + 31;

/// Inputs and results crossing from a worker to the resident message loop.
pub enum PipelineEvent {
    Extraction {
        attempt: u64,
        trigger: TriggerKind,
        process_id: u32,
        pointer: Option<crate::mouse::ScreenPoint>,
        selection_rect: Option<ScreenRect>,
        result: ExtractionResult,
    },
    Delta {
        job_id: u64,
        delta: String,
    },
    Finished {
        job_id: u64,
        result: Result<(), ProviderError>,
    },
}

/// Configuration and provider ownership passed from the resident executable.
/// The Windows platform crate depends only on the portable provider trait;
/// the resident executable adapts the selected concrete provider.
pub struct AppRuntime {
    pub config: AppConfig,
    pub request_gate: RequestGate,
    pub provider: Option<Arc<dyn TranslationProvider + Send + Sync>>,
    pub startup_error: Option<String>,
    pub provider_reloader: ProviderReloader,
    /// Optional asynchronous history sink. The message loop only enqueues
    /// completed entries; the implementation owns persistence scheduling.
    pub history: Option<Arc<dyn HistoryStore>>,
}

/// Provider state rebuilt from one validated application configuration.
/// Keeping the concrete provider construction in the resident executable
/// preserves the platform crate's dependency direction.
pub struct ProviderRuntime {
    pub provider_config: Option<ProviderConfig>,
    pub provider: Option<Arc<dyn TranslationProvider + Send + Sync>>,
    pub error: Option<String>,
}

pub type ProviderReloader = Arc<dyn Fn(&AppConfig) -> ProviderRuntime + Send + Sync>;

pub struct Pipeline {
    extraction_worker: Option<ExtractionWorker>,
    provider: Option<Arc<dyn TranslationProvider + Send + Sync>>,
    events: Sender<PipelineEvent>,
    #[cfg(windows)]
    hwnd: HWND,
    next_attempt: AtomicU64,
}

impl Pipeline {
    /// Construct a pipeline and return the receiver consumed by the message
    /// loop.  UI Automation failure is represented as a platform extraction
    /// error rather than terminating the resident.
    #[cfg(windows)]
    pub fn with_receiver(
        provider: Option<Arc<dyn TranslationProvider + Send + Sync>>,
        hwnd: HWND,
    ) -> (Self, Receiver<PipelineEvent>) {
        let (events, receiver) = mpsc::channel();
        let extractor = UiaExtractor::new().ok().map(Arc::new);
        let clipboard = ClipboardExtractor::new().ok().map(Arc::new);
        let ocr = Some(Arc::new(OcrExtractor::new()));
        let extraction_worker =
            ExtractionWorker::new(extractor, clipboard, ocr, events.clone(), hwnd).ok();
        (
            Self {
                extraction_worker,
                provider,
                events,
                hwnd,
                next_attempt: AtomicU64::new(1),
            },
            receiver,
        )
    }

    pub fn extract(
        &self,
        trigger: TriggerKind,
        process_id: u32,
        source_root_window: isize,
        pointer: Option<crate::mouse::ScreenPoint>,
        selection_rect: Option<ScreenRect>,
    ) -> u64 {
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        let request = ExtractionRequest {
            attempt,
            trigger,
            process_id,
            source_root_window,
            pointer,
            selection_rect,
            cancellation: selection_platform_interface::CancellationToken::new(),
        };

        #[cfg(windows)]
        let submitted = self
            .extraction_worker
            .as_ref()
            .is_some_and(|worker| worker.submit(request));
        #[cfg(not(windows))]
        let submitted = {
            let _ = request;
            false
        };

        #[cfg(windows)]
        runtime_trace::record(if submitted {
            "extraction_submit_success"
        } else {
            "extraction_submit_failure"
        });

        if !submitted {
            post(
                &self.events,
                #[cfg(windows)]
                self.hwnd,
                PipelineEvent::Extraction {
                    attempt,
                    trigger,
                    process_id,
                    pointer,
                    selection_rect,
                    result: Err(ExtractionFailure::Platform),
                },
            );
        }
        attempt
    }

    /// Replace the provider used by subsequently admitted jobs. Existing
    /// workers retain their own `Arc`; the app cancels them before reloading.
    pub fn set_provider(&mut self, provider: Option<Arc<dyn TranslationProvider + Send + Sync>>) {
        self.provider = provider;
    }

    /// Cancel active and pending local extraction without waiting for a
    /// platform API to return. Adapter workers observe the token while the
    /// resident can immediately accept higher-priority work or shut down.
    pub fn cancel_extraction(&self) {
        if let Some(worker) = self.extraction_worker.as_ref() {
            worker.cancel();
        }
    }

    /// Start provider streaming on a worker and return its cancellation token.
    pub fn stream(
        &self,
        job_id: u64,
        request: selection_core::PreparedRequest,
    ) -> CancellationToken {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let events = self.events.clone();
        #[cfg(windows)]
        let hwnd_value = self.hwnd.0 as isize;
        #[cfg(windows)]
        let hwnd = HWND(hwnd_value as *mut _);
        let Some(provider) = self.provider.clone() else {
            post(
                &events,
                #[cfg(windows)]
                hwnd,
                PipelineEvent::Finished {
                    job_id,
                    result: Err(ProviderError::Unavailable),
                },
            );
            return token;
        };
        let failure_events = events.clone();
        let spawn = thread::Builder::new()
            .name("selection-translate-provider".to_owned())
            .spawn(move || {
                #[cfg(windows)]
                let hwnd = HWND(hwnd_value as *mut _);
                let mut sink = WorkerSink {
                    job_id,
                    events: events.clone(),
                    #[cfg(windows)]
                    hwnd,
                };
                let result = provider.stream(&request, &worker_token, &mut sink);
                post(
                    &events,
                    #[cfg(windows)]
                    hwnd,
                    PipelineEvent::Finished { job_id, result },
                );
            });
        if spawn.is_err() {
            post(
                &failure_events,
                #[cfg(windows)]
                self.hwnd,
                PipelineEvent::Finished {
                    job_id,
                    result: Err(ProviderError::Unavailable),
                },
            );
        }
        token
    }
}

/// One extraction request in the bounded dispatcher.
///
/// The resident message loop can produce triggers much faster than UIA or OCR
/// can inspect a target. Keeping the request metadata together means a burst
/// can be replaced atomically without creating a thread or losing the attempt
/// identity used by the app's stale-completion checks.
struct ExtractionRequest {
    attempt: u64,
    trigger: TriggerKind,
    process_id: u32,
    source_root_window: isize,
    pointer: Option<crate::mouse::ScreenPoint>,
    selection_rect: Option<ScreenRect>,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct ExtractionMailbox {
    state: Mutex<ExtractionMailboxState>,
    wake: Condvar,
}

#[derive(Default)]
struct ExtractionMailboxState {
    /// There is deliberately only one pending request. `submit` replaces it,
    /// so an input burst cannot become an unbounded queue.
    pending: Option<ExtractionRequest>,
    active: Option<(u64, CancellationToken)>,
    shutting_down: bool,
}

impl ExtractionMailbox {
    fn submit(&self, request: ExtractionRequest) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.shutting_down {
            return false;
        }
        if let Some((_, cancellation)) = state.active.as_ref() {
            cancellation.cancel();
        }
        state.pending = Some(request);
        self.wake.notify_one();
        true
    }

    /// Wait for the next request. The worker calls this only after the
    /// previous request has completed, giving the dispatcher one active job
    /// and one pending slot at all times.
    fn next(&self) -> Option<ExtractionRequest> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.shutting_down {
                state.pending = None;
                return None;
            }
            if let Some(request) = state.pending.take() {
                state.active = Some((request.attempt, request.cancellation.clone()));
                return Some(request);
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn complete(&self, attempt: u64) {
        if let Ok(mut state) = self.state.lock() {
            if state
                .active
                .as_ref()
                .is_some_and(|(active_attempt, _)| *active_attempt == attempt)
            {
                state.active = None;
            }
        }
    }

    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutting_down = true;
            state.pending = None;
            if let Some((_, cancellation)) = state.active.take() {
                cancellation.cancel();
            }
            self.wake.notify_one();
        }
    }

    fn cancel(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending = None;
            if let Some((_, cancellation)) = state.active.take() {
                cancellation.cancel();
            }
        }
    }
}

/// A single long-lived extraction worker with a one-request coalescing
/// mailbox. UIA, clipboard, and OCR adapters are all invoked sequentially on
/// this worker, so rapid trigger input cannot allocate one thread per trigger.
struct ExtractionWorker {
    mailbox: Arc<ExtractionMailbox>,
    join: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl ExtractionWorker {
    fn new(
        extractor: Option<Arc<UiaExtractor>>,
        clipboard: Option<Arc<ClipboardExtractor>>,
        ocr: Option<Arc<OcrExtractor>>,
        events: Sender<PipelineEvent>,
        hwnd: HWND,
    ) -> Result<Self, ExtractionFailure> {
        let mailbox = Arc::new(ExtractionMailbox::default());
        let worker_mailbox = Arc::clone(&mailbox);
        // HWND wraps a raw pointer and is intentionally not `Send`; carry
        // the value across the thread boundary as an integer, then rebuild
        // the handle on the worker just before posting each event.
        let hwnd_value = hwnd.0 as isize;
        let join = thread::Builder::new()
            .name("selection-translate-extract".to_owned())
            .spawn(move || {
                let hwnd = HWND(hwnd_value as *mut _);
                while let Some(request) = worker_mailbox.next() {
                    let attempt = request.attempt;
                    runtime_trace::record("extraction_worker_begin");
                    run_extraction(
                        request,
                        extractor.as_ref(),
                        clipboard.as_ref(),
                        ocr.as_ref(),
                        &events,
                        hwnd,
                    );
                    worker_mailbox.complete(attempt);
                }
            })
            .map_err(|_| ExtractionFailure::Platform)?;
        Ok(Self {
            mailbox,
            join: Some(join),
        })
    }

    fn submit(&self, request: ExtractionRequest) -> bool {
        self.mailbox.submit(request)
    }

    fn cancel(&self) {
        self.mailbox.cancel();
    }
}

impl Drop for ExtractionWorker {
    fn drop(&mut self) {
        self.mailbox.shutdown();
        if let Some(join) = self.join.take() {
            const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_millis(150);
            const WAIT_SLICE: Duration = Duration::from_millis(5);
            let deadline = Instant::now() + SHUTDOWN_JOIN_BUDGET;
            while !join.is_finished() && Instant::now() < deadline {
                thread::sleep(WAIT_SLICE);
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}

#[cfg(windows)]
fn run_extraction(
    request: ExtractionRequest,
    extractor: Option<&Arc<UiaExtractor>>,
    clipboard: Option<&Arc<ClipboardExtractor>>,
    ocr: Option<&Arc<OcrExtractor>>,
    events: &Sender<PipelineEvent>,
    hwnd: HWND,
) {
    let ExtractionRequest {
        attempt,
        trigger,
        process_id,
        source_root_window,
        pointer,
        selection_rect,
        cancellation,
    } = request;
    if trigger == TriggerKind::Hover {
        runtime_trace::record("hover_uia_begin");
    }
    let uia_result = extractor.map_or(Err(ExtractionFailure::Platform), |extractor| {
        extractor.extract_cancellable(
            trigger,
            process_id,
            source_root_window,
            pointer,
            selection_rect,
            &cancellation,
        )
    });
    let uia_result = sanitize_extraction_result(trigger, uia_result);
    if trigger == TriggerKind::Hover {
        runtime_trace::record("hover_uia_returned");
    }
    if cancellation.is_cancelled() {
        if trigger == TriggerKind::Hover {
            runtime_trace::record("hover_extraction_cancelled_after_uia");
        }
        return;
    }
    record_extraction_stage("uia", trigger, &uia_result);
    let mut result = match trigger {
        TriggerKind::Manual => uia_result
            .or_else(|uia_error| {
                let clipboard_result = clipboard.as_ref().map_or(Err(uia_error), |clipboard| {
                    clipboard.extract_cancellable(process_id, source_root_window, &cancellation)
                });
                record_extraction_stage("clipboard", trigger, &clipboard_result);
                clipboard_result
            })
            .or_else(|error| {
                if cancellation.is_cancelled() {
                    Err(error)
                } else {
                    let ocr_result = ocr.map_or(Err(error), |ocr| {
                        ocr.extract_cancellable(trigger, pointer, selection_rect, &cancellation)
                    });
                    let ocr_result = sanitize_extraction_result(trigger, ocr_result);
                    record_extraction_stage("ocr", trigger, &ocr_result);
                    ocr_result
                }
            }),
        TriggerKind::Selection => {
            debug_assert!(matches!(trigger, TriggerKind::Selection));
            uia_result
                .or_else(|uia_error| {
                    if cancellation.is_cancelled() {
                        return Err(uia_error);
                    }
                    let native_result = native_selection::extract_cancellable(
                        pointer,
                        process_id,
                        selection_rect,
                        &cancellation,
                    );
                    record_extraction_stage("native", trigger, &native_result);
                    match native_result {
                        Err(ExtractionFailure::PermissionDenied) => {
                            Err(ExtractionFailure::PermissionDenied)
                        }
                        result => result,
                    }
                })
                .or_else(|uia_error| {
                    if !native_failure_allows_fallback(uia_error) {
                        return Err(uia_error);
                    }
                    if !clipboard_fallback_allowed(trigger, selection_rect.is_some()) {
                        return Err(uia_error);
                    }
                    let clipboard_result = clipboard.as_ref().map_or(Err(uia_error), |clipboard| {
                        clipboard.extract_cancellable(process_id, source_root_window, &cancellation)
                    });
                    record_extraction_stage("clipboard", trigger, &clipboard_result);
                    clipboard_result
                })
                .or_else(|error| {
                    if cancellation.is_cancelled() || !native_failure_allows_fallback(error) {
                        Err(error)
                    } else {
                        let ocr_result = ocr.map_or(Err(error), |ocr| {
                            ocr.extract_cancellable(trigger, pointer, selection_rect, &cancellation)
                        });
                        record_extraction_stage("ocr", trigger, &ocr_result);
                        ocr_result
                    }
                })
        }
        TriggerKind::Hover => match uia_result {
            Ok(mut primary) => {
                enrich_missing_hover_context(
                    &mut primary,
                    ocr,
                    pointer,
                    selection_rect,
                    &cancellation,
                );
                if hover_sentence(&primary).is_some() {
                    Ok(primary)
                } else {
                    Err(ExtractionFailure::EmptyRange)
                }
            }
            Err(error) => {
                if cancellation.is_cancelled() {
                    Err(error)
                } else {
                    let ocr_result = ocr.map_or(Err(error), |ocr| {
                        ocr.extract_cancellable(trigger, pointer, selection_rect, &cancellation)
                    });
                    let ocr_result = sanitize_extraction_result(trigger, ocr_result);
                    record_extraction_stage("ocr", trigger, &ocr_result);
                    ocr_result
                }
            }
        },
    };
    if trigger == TriggerKind::Selection && !cancellation.is_cancelled() {
        enrich_missing_selection_context(&mut result, ocr, pointer, selection_rect, &cancellation);
    }
    if cancellation.is_cancelled() {
        return;
    }
    post(
        events,
        hwnd,
        PipelineEvent::Extraction {
            attempt,
            trigger,
            process_id,
            pointer,
            selection_rect,
            result,
        },
    );
}

#[cfg(windows)]
fn enrich_missing_selection_context(
    result: &mut ExtractionResult,
    ocr: Option<&Arc<OcrExtractor>>,
    pointer: Option<crate::mouse::ScreenPoint>,
    selection_rect: Option<ScreenRect>,
    cancellation: &CancellationToken,
) {
    let needs_context = result.as_ref().is_ok_and(|text| {
        text.source != ExtractionSource::Ocr && selection_sentence(text).is_none()
    });
    if !needs_context || selection_rect.is_none() {
        return;
    }
    let Some(ocr) = ocr else { return };
    runtime_trace::record("selection_context_ocr_attempt");
    let enrichment = ocr.extract_cancellable(
        TriggerKind::Selection,
        pointer,
        selection_rect,
        cancellation,
    );
    if cancellation.is_cancelled() {
        return;
    }
    if let (Ok(primary), Ok(candidate)) = (result.as_mut(), enrichment) {
        if merge_selection_context(primary, &candidate) {
            runtime_trace::record("selection_context_ocr_success");
            return;
        }
    }
    runtime_trace::record("selection_context_ocr_failure");
}

fn selection_sentence(text: &TextContext) -> Option<String> {
    text.context
        .as_deref()
        .and_then(|context| selection_core::sentence::sentence_for_target(context, &text.target))
}

fn merge_selection_context(primary: &mut TextContext, candidate: &TextContext) -> bool {
    let context = candidate.context.as_deref().and_then(|context| {
        selection_core::sentence::sentence_for_target(context, &primary.target)
    });
    let Some(context) = context else { return false };
    primary.context = Some(context);
    true
}

#[cfg(windows)]
fn enrich_missing_hover_context(
    primary: &mut TextContext,
    ocr: Option<&Arc<OcrExtractor>>,
    pointer: Option<crate::mouse::ScreenPoint>,
    selection_rect: Option<ScreenRect>,
    cancellation: &CancellationToken,
) {
    if hover_sentence(primary).is_some() {
        return;
    }
    let Some(ocr) = ocr else { return };
    let Ok(candidate) = sanitize_extraction_result(
        TriggerKind::Hover,
        ocr.extract_cancellable(TriggerKind::Hover, pointer, selection_rect, cancellation),
    ) else {
        return;
    };
    if cancellation.is_cancelled() {
        return;
    }
    let _ = merge_hover_context(primary, &candidate);
}

/// Validate automatic coordinate text before it can short-circuit fallback.
/// Selection and Manual preserve their existing, intentionally broader text
/// contract; Hover is constrained to one exact token and its local sentence.
fn sanitize_extraction_result(trigger: TriggerKind, result: ExtractionResult) -> ExtractionResult {
    if trigger != TriggerKind::Hover {
        return result;
    }
    result.and_then(|text| {
        selection_core::normalize::sanitize_hover_text_context(text)
            .ok_or(ExtractionFailure::EmptyRange)
    })
}

fn hover_sentence(text: &TextContext) -> Option<String> {
    text.context
        .as_deref()
        .and_then(|context| selection_core::sentence::sentence_for_target(context, &text.target))
}

fn merge_hover_context(primary: &mut TextContext, candidate: &TextContext) -> bool {
    if !same_normalized_target(&primary.target, &candidate.target) {
        return false;
    }
    let context = candidate.context.as_deref().and_then(|context| {
        selection_core::sentence::sentence_for_target(context, &primary.target)
    });
    let Some(context) = context else { return false };
    primary.context = Some(context);
    true
}

fn same_normalized_target(first: &str, second: &str) -> bool {
    let first = selection_core::normalize::normalize_target(first);
    let second = selection_core::normalize::normalize_target(second);
    first == second
        || (first.is_ascii() && second.is_ascii() && first.eq_ignore_ascii_case(&second))
}

/// Record only fixed, privacy-safe extraction stages. The failure category is
/// an enum-to-label mapping; no HRESULT, OS error, text, coordinates, window
/// identity, or other runtime data is emitted.
fn record_extraction_stage(source: &'static str, trigger: TriggerKind, result: &ExtractionResult) {
    let label = match (source, trigger, result) {
        ("uia", TriggerKind::Selection, Ok(_)) => "selection_uia_success",
        ("uia", TriggerKind::Selection, Err(ExtractionFailure::UnsupportedPattern)) => {
            "selection_uia_failure_unsupported"
        }
        ("uia", TriggerKind::Selection, Err(ExtractionFailure::EmptyRange)) => {
            "selection_uia_failure_empty"
        }
        ("uia", TriggerKind::Selection, Err(ExtractionFailure::PermissionDenied)) => {
            "selection_uia_failure_permission"
        }
        ("uia", TriggerKind::Selection, Err(ExtractionFailure::StaleElement)) => {
            "selection_uia_failure_stale"
        }
        ("uia", TriggerKind::Selection, Err(ExtractionFailure::Platform)) => {
            "selection_uia_failure_platform"
        }
        ("native", TriggerKind::Selection, Ok(_)) => "selection_native_success",
        ("native", TriggerKind::Selection, Err(ExtractionFailure::UnsupportedPattern)) => {
            "selection_native_failure_unsupported"
        }
        ("native", TriggerKind::Selection, Err(ExtractionFailure::EmptyRange)) => {
            "selection_native_failure_empty"
        }
        ("native", TriggerKind::Selection, Err(ExtractionFailure::PermissionDenied)) => {
            "selection_native_failure_permission"
        }
        ("native", TriggerKind::Selection, Err(ExtractionFailure::StaleElement)) => {
            "selection_native_failure_stale"
        }
        ("native", TriggerKind::Selection, Err(ExtractionFailure::Platform)) => {
            "selection_native_failure_platform"
        }
        ("clipboard", TriggerKind::Selection, Ok(_)) => "selection_clipboard_success",
        ("clipboard", TriggerKind::Selection, Err(ExtractionFailure::UnsupportedPattern)) => {
            "selection_clipboard_failure_unsupported"
        }
        ("clipboard", TriggerKind::Selection, Err(ExtractionFailure::EmptyRange)) => {
            "selection_clipboard_failure_empty"
        }
        ("clipboard", TriggerKind::Selection, Err(ExtractionFailure::PermissionDenied)) => {
            "selection_clipboard_failure_permission"
        }
        ("clipboard", TriggerKind::Selection, Err(ExtractionFailure::StaleElement)) => {
            "selection_clipboard_failure_stale"
        }
        ("clipboard", TriggerKind::Selection, Err(ExtractionFailure::Platform)) => {
            "selection_clipboard_failure_platform"
        }
        ("ocr", TriggerKind::Selection, Ok(_)) => "selection_ocr_success",
        ("ocr", TriggerKind::Selection, Err(ExtractionFailure::UnsupportedPattern)) => {
            "selection_ocr_failure_unsupported"
        }
        ("ocr", TriggerKind::Selection, Err(ExtractionFailure::EmptyRange)) => {
            "selection_ocr_failure_empty"
        }
        ("ocr", TriggerKind::Selection, Err(ExtractionFailure::PermissionDenied)) => {
            "selection_ocr_failure_permission"
        }
        ("ocr", TriggerKind::Selection, Err(ExtractionFailure::StaleElement)) => {
            "selection_ocr_failure_stale"
        }
        ("ocr", TriggerKind::Selection, Err(ExtractionFailure::Platform)) => {
            "selection_ocr_failure_platform"
        }
        ("uia", TriggerKind::Manual, Ok(_)) => "manual_uia_success",
        ("uia", TriggerKind::Manual, Err(_)) => "manual_uia_failure",
        ("clipboard", TriggerKind::Manual, Ok(_)) => "manual_clipboard_success",
        ("clipboard", TriggerKind::Manual, Err(_)) => "manual_clipboard_failure",
        ("ocr", TriggerKind::Manual, Ok(_)) => "manual_ocr_success",
        ("ocr", TriggerKind::Manual, Err(_)) => "manual_ocr_failure",
        ("uia", TriggerKind::Hover, Ok(_)) => "hover_uia_success",
        ("uia", TriggerKind::Hover, Err(_)) => "hover_uia_failure",
        ("ocr", TriggerKind::Hover, Ok(_)) => "hover_ocr_success",
        ("ocr", TriggerKind::Hover, Err(_)) => "hover_ocr_failure",
        _ => return,
    };
    runtime_trace::record(label);
    if let Err(error) = result {
        let category = match (trigger, error) {
            (TriggerKind::Selection, ExtractionFailure::UnsupportedPattern) => {
                "selection_failure_unsupported"
            }
            (TriggerKind::Selection, ExtractionFailure::EmptyRange) => "selection_failure_empty",
            (TriggerKind::Selection, ExtractionFailure::PermissionDenied) => {
                "selection_failure_permission"
            }
            (TriggerKind::Selection, ExtractionFailure::StaleElement) => "selection_failure_stale",
            (TriggerKind::Selection, ExtractionFailure::Platform) => "selection_failure_platform",
            (TriggerKind::Manual, ExtractionFailure::UnsupportedPattern) => {
                "manual_failure_unsupported"
            }
            (TriggerKind::Manual, ExtractionFailure::EmptyRange) => "manual_failure_empty",
            (TriggerKind::Manual, ExtractionFailure::PermissionDenied) => {
                "manual_failure_permission"
            }
            (TriggerKind::Manual, ExtractionFailure::StaleElement) => "manual_failure_stale",
            (TriggerKind::Manual, ExtractionFailure::Platform) => "manual_failure_platform",
            (TriggerKind::Hover, ExtractionFailure::UnsupportedPattern) => {
                "hover_failure_unsupported"
            }
            (TriggerKind::Hover, ExtractionFailure::EmptyRange) => "hover_failure_empty",
            (TriggerKind::Hover, ExtractionFailure::PermissionDenied) => "hover_failure_permission",
            (TriggerKind::Hover, ExtractionFailure::StaleElement) => "hover_failure_stale",
            (TriggerKind::Hover, ExtractionFailure::Platform) => "hover_failure_platform",
        };
        runtime_trace::record(category);
    }
}

/// Clipboard Copy is safe for explicit Manual and automatic Selection
/// requests, but must never be synthesized by opt-in Hover.
fn clipboard_fallback_allowed(trigger: TriggerKind, has_selection_rect: bool) -> bool {
    matches!(trigger, TriggerKind::Manual)
        || (trigger == TriggerKind::Selection && has_selection_rect)
}

fn native_failure_allows_fallback(error: ExtractionFailure) -> bool {
    !matches!(error, ExtractionFailure::PermissionDenied)
}

struct WorkerSink {
    job_id: u64,
    events: Sender<PipelineEvent>,
    #[cfg(windows)]
    hwnd: HWND,
}

impl PopupSink for WorkerSink {
    fn show_loading(&mut self, _job_id: u64) {}

    fn update(&mut self, job_id: u64, delta: &str) {
        if job_id == self.job_id && !delta.is_empty() {
            post(
                &self.events,
                #[cfg(windows)]
                self.hwnd,
                PipelineEvent::Delta {
                    job_id,
                    delta: delta.to_owned(),
                },
            );
        }
    }

    fn finish(&mut self, _job_id: u64) {}
    fn show_local_error(&mut self, _job_id: u64, _message: &str) {}
    fn dismiss(&mut self, _job_id: u64) {}
}

#[cfg(windows)]
fn post(events: &Sender<PipelineEvent>, hwnd: HWND, event: PipelineEvent) {
    let _ = events.send(event);
    unsafe {
        let _ = PostMessageW(Some(hwnd), PIPELINE_EVENT, WPARAM(0), LPARAM(0));
    }
}

#[cfg(not(windows))]
fn post(events: &Sender<PipelineEvent>, event: PipelineEvent) {
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::{
        clipboard_fallback_allowed, hover_sentence, merge_hover_context, merge_selection_context,
        native_failure_allows_fallback, sanitize_extraction_result, selection_sentence,
        ExtractionMailbox, ExtractionRequest,
    };
    use selection_core::{ExtractionSource, ScreenRect, TextContext, TriggerKind};
    use selection_platform_interface::ScreenPoint;

    #[test]
    fn clipboard_fallback_routing_allows_selection_and_manual_but_rejects_hover() {
        assert!(clipboard_fallback_allowed(TriggerKind::Selection, true));
        assert!(!clipboard_fallback_allowed(TriggerKind::Selection, false));
        assert!(clipboard_fallback_allowed(TriggerKind::Manual, false));
        assert!(!clipboard_fallback_allowed(TriggerKind::Hover, true));
    }

    #[test]
    fn sensitive_native_failure_is_terminal_before_clipboard_or_ocr() {
        assert!(!native_failure_allows_fallback(
            selection_platform_interface::ExtractionFailure::PermissionDenied
        ));
        assert!(native_failure_allows_fallback(
            selection_platform_interface::ExtractionFailure::UnsupportedPattern
        ));
    }

    #[test]
    fn context_enrichment_preserves_selected_target_and_adds_its_sentence() {
        let mut primary = TextContext::new("bank", ExtractionSource::Clipboard);
        let candidate = TextContext {
            target: "bank".to_owned(),
            context: Some("He sat on the bank by the river. Then he left.".to_owned()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };

        assert!(merge_selection_context(&mut primary, &candidate));
        assert_eq!(primary.target, "bank");
        assert_eq!(primary.source, ExtractionSource::Clipboard);
        assert_eq!(
            primary.context.as_deref(),
            Some("He sat on the bank by the river.")
        );
        assert_eq!(selection_sentence(&primary), primary.context);
    }

    #[test]
    fn unrelated_ocr_text_cannot_be_attached_as_selection_context() {
        let mut primary = TextContext::new("bank", ExtractionSource::Clipboard);
        let candidate = TextContext {
            target: "bench".to_owned(),
            context: Some("He sat on the bench.".to_owned()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };

        assert!(!merge_selection_context(&mut primary, &candidate));
        assert!(primary.context.is_none());
    }

    #[test]
    fn hover_context_enrichment_preserves_uia_target_and_source() {
        let mut primary = TextContext {
            target: "bank".into(),
            context: None,
            source: ExtractionSource::UiaPoint,
            screen_rect: Some(ScreenRect::new(10, 20, 30, 40)),
        };
        let candidate = TextContext {
            target: "bank".into(),
            context: Some("He sat on the bank by the river.".into()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };
        assert!(merge_hover_context(&mut primary, &candidate));
        assert_eq!(primary.target, "bank");
        assert_eq!(primary.source, ExtractionSource::UiaPoint);
        assert_eq!(primary.screen_rect, Some(ScreenRect::new(10, 20, 30, 40)));
        assert_eq!(
            hover_sentence(&primary).as_deref(),
            Some("He sat on the bank by the river.")
        );
    }

    #[test]
    fn hover_context_mismatch_is_rejected_without_replacing_target() {
        let mut primary = TextContext::new("bank", ExtractionSource::UiaPoint);
        let candidate = TextContext {
            target: "bench".into(),
            context: Some("He sat on the bench.".into()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };
        assert!(!merge_hover_context(&mut primary, &candidate));
        assert_eq!(primary.target, "bank");
        assert!(hover_sentence(&primary).is_none());
    }

    #[test]
    fn hover_context_rejects_a_different_ocr_target_even_if_sentence_mentions_primary() {
        let mut primary = TextContext::new("bank", ExtractionSource::UiaPoint);
        let candidate = TextContext {
            target: "river".into(),
            context: Some("He sat on the bank by the river.".into()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };
        assert!(!merge_hover_context(&mut primary, &candidate));
        assert!(primary.context.is_none());
    }

    #[test]
    fn hover_without_containing_sentence_fails_locally() {
        let primary = TextContext::new("word", ExtractionSource::UiaPoint);
        assert!(hover_sentence(&primary).is_none());
    }

    #[test]
    fn hover_extractor_result_is_cleaned_before_it_can_stop_fallback() {
        let result = sanitize_extraction_result(
            TriggerKind::Hover,
            Ok(TextContext {
                target: "• **word,**".into(),
                context: Some("Read **word,** in this sentence.".into()),
                source: ExtractionSource::UiaPoint,
                screen_rect: Some(ScreenRect::new(1, 2, 3, 4)),
            }),
        )
        .expect("decorated Hover word remains usable");
        assert_eq!(result.target, "word");
        assert_eq!(
            result.context.as_deref(),
            Some("Read **word,** in this sentence.")
        );
        assert_eq!(result.screen_rect, Some(ScreenRect::new(1, 2, 3, 4)));
    }

    #[test]
    fn hover_junk_becomes_empty_range_but_selection_remains_unchanged() {
        let junk = TextContext::new("foo***bar", ExtractionSource::UiaPoint);
        assert_eq!(
            sanitize_extraction_result(TriggerKind::Hover, Ok(junk.clone())),
            Err(selection_platform_interface::ExtractionFailure::EmptyRange)
        );
        assert_eq!(
            sanitize_extraction_result(TriggerKind::Selection, Ok(junk.clone())),
            Ok(junk)
        );
    }

    fn request(attempt: u64, trigger: TriggerKind) -> ExtractionRequest {
        ExtractionRequest {
            attempt,
            trigger,
            process_id: attempt as u32,
            source_root_window: attempt as isize,
            pointer: Some(ScreenPoint::new(attempt as i32, 10)),
            selection_rect: Some(ScreenRect::new(1, 2, 3, 4)),
            cancellation: selection_platform_interface::CancellationToken::new(),
        }
    }

    #[test]
    fn mailbox_coalesces_a_burst_to_the_newest_pending_request() {
        let mailbox = ExtractionMailbox::default();
        for attempt in 1..=1_000 {
            assert!(mailbox.submit(request(attempt, TriggerKind::Hover)));
        }

        let state = mailbox.state.lock().expect("mailbox state");
        let pending = state.pending.as_ref().expect("one pending request");
        assert_eq!(pending.attempt, 1_000);
        assert_eq!(pending.process_id, 1_000);
        assert_eq!(pending.pointer, Some(ScreenPoint::new(1_000, 10)));
    }

    #[test]
    fn newer_work_cancels_the_active_extraction() {
        let mailbox = ExtractionMailbox::default();
        assert!(mailbox.submit(request(1, TriggerKind::Hover)));
        let active = mailbox.next().expect("active request");
        assert!(!active.cancellation.is_cancelled());

        assert!(mailbox.submit(request(2, TriggerKind::Manual)));
        assert!(active.cancellation.is_cancelled());
        mailbox.complete(active.attempt);

        let replacement = mailbox.next().expect("replacement request");
        assert_eq!(replacement.attempt, 2);
        assert!(!replacement.cancellation.is_cancelled());
    }

    #[test]
    fn shutdown_discards_pending_work_and_wakes_the_worker() {
        let mailbox = ExtractionMailbox::default();
        assert!(mailbox.submit(request(1, TriggerKind::Selection)));
        mailbox.shutdown();

        assert!(mailbox.next().is_none());
        let state = mailbox.state.lock().expect("mailbox state");
        assert!(state.pending.is_none());
        assert!(state.active.is_none());
        assert!(state.shutting_down);
    }
}

#[cfg(all(test, windows))]
mod end_to_end_tests {
    use super::*;
    use selection_core::{
        ExtractionSource, JobInput, PromptConfig, ProviderConfig, RequestGate, TextContext,
        TriggerKind,
    };
    use selection_platform_interface::{ProviderError, ProviderResult};
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::HWND;

    /// A provider that never performs I/O. The first request can be held until
    /// its cancellation token is set, which makes supersession deterministic.
    struct MockProvider {
        calls: AtomicUsize,
        job_ids: Mutex<Vec<u64>>,
        first_started: AtomicBool,
        hold_first: bool,
    }

    impl MockProvider {
        fn new(hold_first: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                job_ids: Mutex::new(Vec::new()),
                first_started: AtomicBool::new(false),
                hold_first,
            }
        }
    }

    impl TranslationProvider for MockProvider {
        fn stream(
            &self,
            prepared: &selection_platform_interface::PreparedRequest,
            cancellation: &CancellationToken,
            sink: &mut dyn PopupSink,
        ) -> ProviderResult {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.job_ids
                .lock()
                .expect("mock provider job ids")
                .push(prepared.job_id());

            if self.hold_first && prepared.job_id() == 1 {
                self.first_started.store(true, Ordering::Release);
                while !cancellation.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                return Err(ProviderError::Cancelled);
            }

            sink.update(prepared.job_id(), "translated ");
            sink.update(prepared.job_id(), "result");
            Ok(())
        }
    }

    fn test_pipeline(
        provider: Arc<dyn TranslationProvider + Send + Sync>,
    ) -> (Pipeline, Receiver<PipelineEvent>) {
        let (events, receiver) = mpsc::channel();
        let pipeline = Pipeline {
            extraction_worker: None,
            provider: Some(provider),
            events,
            hwnd: HWND::default(),
            next_attempt: AtomicU64::new(1),
        };
        (pipeline, receiver)
    }

    fn gate() -> RequestGate {
        RequestGate::new(
            ProviderConfig::new("http://127.0.0.1:1", "mock-model"),
            [PromptConfig::new("translate")],
        )
    }

    fn extracted_input(id: u64, target: &str) -> JobInput {
        JobInput::new(
            id,
            TriggerKind::Selection,
            TextContext {
                target: target.to_owned(),
                context: Some("the complete source sentence".to_owned()),
                source: ExtractionSource::UiaSelection,
                screen_rect: None,
            },
            "translate",
        )
    }

    fn wait_for_finished(receiver: &Receiver<PipelineEvent>, expected_job: u64) -> (String, usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = String::new();
        let mut finished = 0;
        while Instant::now() < deadline {
            let event = receiver
                .recv_timeout(Duration::from_millis(100))
                .expect("mock pipeline event");
            match event {
                PipelineEvent::Delta { job_id, delta } if job_id == expected_job => {
                    output.push_str(&delta);
                }
                PipelineEvent::Finished { job_id, result } if job_id == expected_job => {
                    assert!(result.is_ok(), "mock provider failed: {result:?}");
                    finished += 1;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(finished, 1, "expected one completed popup stream");
        (output, finished)
    }

    #[test]
    fn valid_extracted_text_makes_one_request_and_one_completed_popup_stream() {
        let provider = Arc::new(MockProvider::new(false));
        let (pipeline, receiver) = test_pipeline(provider.clone());
        let request = gate()
            .prepare(&extracted_input(1, "translate this word"), 1, false)
            .expect("extracted text should pass the request gate");

        pipeline.stream(request.job_id(), request);
        let (output, finished) = wait_for_finished(&receiver, 1);

        assert_eq!(output, "translated result");
        assert_eq!(finished, 1);
        assert_eq!(provider.calls.load(Ordering::Acquire), 1);
        assert_eq!(
            provider
                .job_ids
                .lock()
                .expect("mock provider job ids")
                .as_slice(),
            &[1]
        );
    }

    #[test]
    fn missing_extracted_text_makes_zero_provider_requests() {
        let provider = Arc::new(MockProvider::new(false));
        let empty = extracted_input(1, "\u{200b}\u{2060}\u{feff}  \n");

        let rejection = gate().prepare(&empty, 1, false).unwrap_err();
        assert_eq!(rejection, selection_core::RequestRejection::MissingTarget);
        assert_eq!(provider.calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn superseded_stream_is_cancelled_and_only_current_popup_result_is_used() {
        let provider = Arc::new(MockProvider::new(true));
        let (pipeline, receiver) = test_pipeline(provider.clone());
        let first = gate()
            .prepare(&extracted_input(1, "first"), 1, false)
            .expect("first text should pass the request gate");
        let second = gate()
            .prepare(&extracted_input(2, "second"), 2, false)
            .expect("second text should pass the request gate");

        let first_token = pipeline.stream(first.job_id(), first);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !provider.first_started.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(provider.first_started.load(Ordering::Acquire));
        first_token.cancel();
        pipeline.stream(second.job_id(), second);

        let mut second_output = String::new();
        let mut first_cancelled = false;
        let mut second_finished = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && (!first_cancelled || second_finished != 1) {
            match receiver
                .recv_timeout(Duration::from_millis(100))
                .expect("superseded pipeline event")
            {
                PipelineEvent::Delta { job_id: 2, delta } => second_output.push_str(&delta),
                PipelineEvent::Delta { job_id: 1, .. } => {
                    panic!("cancelled job emitted a popup delta")
                }
                PipelineEvent::Finished { job_id: 1, result } => {
                    assert_eq!(result, Err(ProviderError::Cancelled));
                    first_cancelled = true;
                }
                PipelineEvent::Finished { job_id: 2, result } => {
                    assert!(result.is_ok(), "current job failed: {result:?}");
                    second_finished += 1;
                }
                _ => {}
            }
        }

        assert!(first_cancelled);
        assert_eq!(second_finished, 1);
        assert_eq!(second_output, "translated result");
        assert_eq!(provider.calls.load(Ordering::Acquire), 2);
        let mut ids = provider
            .job_ids
            .lock()
            .expect("mock provider job ids")
            .clone();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }
}
