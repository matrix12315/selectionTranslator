//! UI Automation text extraction.
//!
//! UI Automation COM objects are apartment-bound. The extractor therefore
//! owns one long-lived COM MTA worker and sends small, synchronous requests to
//! it. The resident thread remains an ordinary Win32 message-loop thread and
//! never calls UI Automation directly.

#[cfg(windows)]
mod context;
#[cfg(windows)]
mod point;
#[cfg(windows)]
mod selection;

#[cfg(windows)]
mod worker {
    use super::point;
    use super::selection;
    use selection_core::TriggerKind;
    use selection_platform_interface::{
        CancellationToken, ExtractionFailure, ExtractionResult, ScreenPoint, ScreenRect,
    };
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
    use std::sync::Mutex;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation8, IUIAutomation};

    enum Request {
        Extract {
            trigger: TriggerKind,
            process_id: u32,
            source_root_window: isize,
            pointer: Option<ScreenPoint>,
            selection_rect: Option<ScreenRect>,
            response: SyncSender<ExtractionResult>,
        },
        Shutdown,
    }

    fn request_mailbox() -> (SyncSender<Request>, Receiver<Request>) {
        // One bounded slot avoids the race caused by try_send on a
        // zero-capacity rendezvous channel while preventing queue growth.
        mpsc::sync_channel(1)
    }

    /// UI Automation extractor backed by a dedicated COM MTA worker.
    pub struct UiaExtractor {
        requests: SyncSender<Request>,
        worker: Mutex<Option<JoinHandle<()>>>,
    }

    const UIA_REQUEST_TIMEOUT: Duration = Duration::from_millis(750);
    const WAIT_SLICE: Duration = Duration::from_millis(20);
    const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_millis(100);

    impl UiaExtractor {
        /// Start the worker and create `CUIAutomation8` on that worker.
        pub fn new() -> Result<Self, ExtractionFailure> {
            let (requests, incoming) = request_mailbox();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name("selection-translate-uia".to_owned())
                .spawn(move || worker_main(incoming, ready_tx))
                .map_err(|_| ExtractionFailure::Platform)?;
            ready_rx
                .recv_timeout(UIA_REQUEST_TIMEOUT)
                .map_err(|_| ExtractionFailure::Platform)??;
            Ok(Self {
                requests,
                worker: Mutex::new(Some(worker)),
            })
        }

        pub(crate) fn extract_cancellable(
            &self,
            trigger: TriggerKind,
            process_id: u32,
            source_root_window: isize,
            pointer: Option<ScreenPoint>,
            selection_rect: Option<ScreenRect>,
            cancellation: &CancellationToken,
        ) -> ExtractionResult {
            if cancellation.is_cancelled() {
                return Err(ExtractionFailure::Platform);
            }
            let (response_tx, response_rx) = mpsc::sync_channel(1);
            match self.requests.try_send(Request::Extract {
                trigger,
                process_id,
                source_root_window,
                pointer,
                selection_rect,
                response: response_tx,
            }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    return Err(ExtractionFailure::Platform);
                }
            }
            let deadline = Instant::now() + UIA_REQUEST_TIMEOUT;
            loop {
                if cancellation.is_cancelled() {
                    return Err(ExtractionFailure::Platform);
                }
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Err(ExtractionFailure::Platform);
                };
                match response_rx.recv_timeout(remaining.min(WAIT_SLICE)) {
                    Ok(result) => return result,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(ExtractionFailure::Platform);
                    }
                }
            }
        }
    }

    impl selection_platform_interface::TextExtractor for UiaExtractor {
        fn extract(
            &self,
            trigger: TriggerKind,
            pointer: Option<ScreenPoint>,
            selection_rect: Option<ScreenRect>,
        ) -> ExtractionResult {
            self.extract_cancellable(
                trigger,
                0,
                0,
                pointer,
                selection_rect,
                &CancellationToken::new(),
            )
        }
    }

    impl Drop for UiaExtractor {
        fn drop(&mut self) {
            let _ = self.requests.try_send(Request::Shutdown);
            if let Ok(mut worker) = self.worker.lock() {
                if let Some(worker) = worker.take() {
                    let deadline = Instant::now() + SHUTDOWN_JOIN_BUDGET;
                    while !worker.is_finished() && Instant::now() < deadline {
                        thread::sleep(WAIT_SLICE.min(SHUTDOWN_JOIN_BUDGET));
                    }
                    if worker.is_finished() {
                        let _ = worker.join();
                    }
                }
            }
        }
    }

    fn worker_main(
        incoming: Receiver<Request>,
        ready: mpsc::SyncSender<Result<(), ExtractionFailure>>,
    ) {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            let _ = ready.send(Err(ExtractionFailure::Platform));
            return;
        }

        let automation = unsafe {
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
        };
        let automation = match automation {
            Ok(automation) => automation,
            Err(error) => {
                let _ = ready.send(Err(map_error(&error)));
                unsafe { CoUninitialize() };
                return;
            }
        };

        let _ = ready.send(Ok(()));
        while let Ok(request) = incoming.recv() {
            match request {
                Request::Extract {
                    trigger,
                    process_id,
                    source_root_window,
                    pointer,
                    selection_rect,
                    response,
                } => {
                    let result = match trigger {
                        TriggerKind::Selection | TriggerKind::Manual => selection::extract(
                            &automation,
                            process_id,
                            source_root_window,
                            pointer,
                            selection_rect,
                        ),
                        TriggerKind::Hover => match pointer {
                            Some(pointer) => {
                                point::extract(&automation, process_id, source_root_window, pointer)
                            }
                            None => Err(ExtractionFailure::EmptyRange),
                        },
                    };
                    let _ = response.send(result);
                }
                Request::Shutdown => break,
            }
        }
        drop(automation);
        unsafe { CoUninitialize() };
    }

    /// Translate common UIA HRESULTs into the portable failure contract.
    pub(super) fn map_error(error: &windows::core::Error) -> ExtractionFailure {
        match error.code().0 as u32 {
            0x8007_0005 => ExtractionFailure::PermissionDenied, // E_ACCESSDENIED
            0x8004_0201 | 0x8001_0108 => ExtractionFailure::StaleElement,
            0x8004_0204 => ExtractionFailure::UnsupportedPattern,
            _ => ExtractionFailure::Platform,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc::{self, TrySendError};

        #[test]
        fn request_mailbox_admits_first_and_rejects_second_until_drained() {
            let (requests, incoming) = request_mailbox();
            let (first_response, _first_result) = mpsc::sync_channel(1);
            let (second_response, _second_result) = mpsc::sync_channel(1);

            requests
                .try_send(Request::Extract {
                    trigger: TriggerKind::Manual,
                    process_id: 42,
                    source_root_window: 42,
                    pointer: None,
                    selection_rect: None,
                    response: first_response,
                })
                .expect("the first extraction request must be admitted");
            assert!(matches!(
                requests.try_send(Request::Extract {
                    trigger: TriggerKind::Manual,
                    process_id: 43,
                    source_root_window: 43,
                    pointer: None,
                    selection_rect: None,
                    response: second_response,
                }),
                Err(TrySendError::Full(_))
            ));

            let _ = incoming.recv().expect("the first request remains queued");
            let (third_response, _third_result) = mpsc::sync_channel(1);
            requests
                .try_send(Request::Extract {
                    trigger: TriggerKind::Manual,
                    process_id: 44,
                    source_root_window: 44,
                    pointer: None,
                    selection_rect: None,
                    response: third_response,
                })
                .expect("the slot must be reusable after the first request is received");
        }
    }
}

#[cfg(windows)]
pub use worker::UiaExtractor;

#[cfg(not(windows))]
mod unsupported {
    use selection_core::TriggerKind;
    use selection_platform_interface::{
        ExtractionFailure, ExtractionResult, ScreenPoint, ScreenRect, TextExtractor,
    };

    /// Placeholder on non-Windows hosts; the first adapter is Windows-only.
    pub struct UiaExtractor;

    impl UiaExtractor {
        pub fn new() -> Result<Self, ExtractionFailure> {
            Err(ExtractionFailure::Platform)
        }
    }

    impl TextExtractor for UiaExtractor {
        fn extract(
            &self,
            _trigger: TriggerKind,
            _pointer: Option<ScreenPoint>,
            _selection_rect: Option<ScreenRect>,
        ) -> ExtractionResult {
            Err(ExtractionFailure::Platform)
        }
    }
}

#[cfg(not(windows))]
pub use unsupported::UiaExtractor;
