//! Manual/Selection clipboard extraction with a bounded, independent clipboard snapshot.

use selection_core::{ExtractionSource, TextContext, TriggerKind};
use selection_platform_interface::{
    ExtractionFailure, ExtractionResult, ScreenPoint, ScreenRect, TextExtractor,
};

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use selection_platform_interface::CancellationToken;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
    use std::sync::Mutex;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::Foundation::{
        GetLastError, GlobalFree, SetLastError, HANDLE, HGLOBAL, HWND, WIN32_ERROR,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
        GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::{
        OleDuplicateData, OleInitialize, OleUninitialize, CF_BITMAP, CF_DSPBITMAP,
        CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_ENHMETAFILE, CF_GDIOBJFIRST, CF_GDIOBJLAST,
        CF_METAFILEPICT, CF_OWNERDISPLAY, CF_PALETTE, CF_PRIVATEFIRST, CF_PRIVATELAST,
        CF_UNICODETEXT, CLIPBOARD_FORMAT,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        VK_C, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId,
        GA_ROOT, HWND_MESSAGE,
    };

    const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(250);
    const INPUT_TIMEOUT: Duration = Duration::from_millis(150);
    const COPY_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(250);
    const WAIT_SLICE: Duration = Duration::from_millis(10);
    const COPY_SETTLE: Duration = Duration::from_millis(20);
    const RESTORE_TIMEOUT: Duration = Duration::from_millis(500);
    // The caller must outlive the full copy deadline and the restoration
    // guard, with room for the worker to be scheduled between those phases.
    const REQUEST_TIMEOUT: Duration = Duration::from_millis(1_800);
    const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_millis(100);
    const MAX_CLIPBOARD_UNITS: usize = 1_000_000;
    const MAX_SNAPSHOT_FORMATS: usize = 128;
    const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
    const MODIFIER_KEYS: [i32; 8] = [
        VK_LCONTROL.0 as i32,
        VK_RCONTROL.0 as i32,
        VK_LMENU.0 as i32,
        VK_RMENU.0 as i32,
        VK_LSHIFT.0 as i32,
        VK_RSHIFT.0 as i32,
        VK_LWIN.0 as i32,
        VK_RWIN.0 as i32,
    ];

    enum Request {
        Extract {
            expected_process_id: u32,
            expected_root_window: isize,
            response: SyncSender<ExtractionResult>,
        },
        Shutdown,
    }

    fn request_mailbox() -> (SyncSender<Request>, Receiver<Request>) {
        // One bounded slot avoids the race caused by try_send on a
        // zero-capacity rendezvous channel while preventing queue growth.
        mpsc::sync_channel(1)
    }

    /// Clipboard access stays on one OLE STA for the lifetime of this worker.
    pub struct ClipboardExtractor {
        requests: SyncSender<Request>,
        worker: Mutex<Option<JoinHandle<()>>>,
    }

    impl ClipboardExtractor {
        pub fn new() -> Result<Self, ExtractionFailure> {
            let (requests, incoming) = request_mailbox();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name("selection-translate-clipboard".to_owned())
                .spawn(move || worker_main(incoming, ready_tx))
                .map_err(|_| ExtractionFailure::Platform)?;
            ready_rx
                .recv_timeout(REQUEST_TIMEOUT)
                .map_err(|_| ExtractionFailure::Platform)??;
            Ok(Self {
                requests,
                worker: Mutex::new(Some(worker)),
            })
        }

        pub(crate) fn extract_cancellable(
            &self,
            expected_process_id: u32,
            expected_root_window: isize,
            cancellation: &CancellationToken,
        ) -> ExtractionResult {
            if cancellation.is_cancelled() {
                return Err(ExtractionFailure::Platform);
            }
            let (response_tx, response_rx) = mpsc::sync_channel(1);
            match self.requests.try_send(Request::Extract {
                expected_process_id,
                expected_root_window,
                response: response_tx,
            }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    return Err(ExtractionFailure::Platform);
                }
            }
            let deadline = Instant::now() + REQUEST_TIMEOUT;
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

    impl TextExtractor for ClipboardExtractor {
        fn extract(
            &self,
            trigger: TriggerKind,
            _pointer: Option<ScreenPoint>,
            _selection_rect: Option<ScreenRect>,
        ) -> ExtractionResult {
            if !clipboard_fallback_allowed(trigger) {
                return Err(ExtractionFailure::UnsupportedPattern);
            }
            self.extract_cancellable(0, 0, &CancellationToken::new())
        }
    }

    impl Drop for ClipboardExtractor {
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
        if unsafe { OleInitialize(None) }.is_err() {
            let _ = ready.send(Err(ExtractionFailure::Platform));
            return;
        }
        let _ = ready.send(Ok(()));
        while let Ok(request) = incoming.recv() {
            match request {
                Request::Extract {
                    expected_process_id,
                    expected_root_window,
                    response,
                } => {
                    let _ = response.send(extract_once(expected_process_id, expected_root_window));
                }
                Request::Shutdown => break,
            }
        }
        unsafe { OleUninitialize() };
    }

    fn extract_once(expected_process_id: u32, expected_root_window: isize) -> ExtractionResult {
        // Do not synthesize Copy unless every advertised original format has
        // been independently and safely cloned; otherwise a failure could
        // permanently replace part of the user's clipboard.
        let snapshot = snapshot_original(Instant::now() + SNAPSHOT_TIMEOUT)?;
        crate::runtime_trace::record("clipboard_snapshot_done");
        let original_sequence = snapshot.sequence;
        let original_owner = snapshot.owner;
        let expected_source = SourceIdentity {
            process_id: expected_process_id,
            root_window: expected_root_window,
        };
        let mut restore = RestoreClipboard::new(snapshot);

        if let Err(error) = send_copy(expected_source, Instant::now() + INPUT_TIMEOUT, None) {
            // SendInput can report a partial injection. Give a completed copy
            // a chance to become visible, then let the guard restore only a
            // stable sequence owned by the copy burst.
            if may_have_produced_copy(&error) {
                let CopySendError::MayHaveCopied(error) = error else {
                    unreachable!("copy failure classification changed");
                };
                if let Some(copy) =
                    wait_for_copy_settle(original_sequence, Instant::now() + COPY_ATTEMPT_TIMEOUT)
                {
                    restore.observe_copy(copy);
                }
                return Err(error);
            }
            let CopySendError::NoInput(error) = error else {
                unreachable!("copy failure classification changed");
            };
            return Err(error);
        }

        let mut copy =
            wait_for_copy_settle(original_sequence, Instant::now() + COPY_ATTEMPT_TIMEOUT);
        let root_matches = source_identity_matches(expected_source, foreground_source_identity());
        if expected_root_window != 0 && !root_matches {
            crate::runtime_trace::record("clipboard_root_mismatch");
        }
        if copy.is_none()
            && root_matches
            && copy_retry_allowed(
                expected_source,
                foreground_source_identity(),
                ClipboardSignals {
                    sequence: original_sequence,
                    owner: original_owner,
                },
                ClipboardSignals {
                    sequence: unsafe { GetClipboardSequenceNumber() },
                    owner: clipboard_owner(),
                },
            )
        {
            // A slow-but-cooperative application gets one bounded retry. The
            // original clipboard signals are rechecked immediately before
            // the second injection so unrelated clipboard changes cannot be
            // overwritten during restoration.
            crate::runtime_trace::record("clipboard_retry");
            match send_copy(
                expected_source,
                Instant::now() + INPUT_TIMEOUT,
                Some(ClipboardSignals {
                    sequence: original_sequence,
                    owner: original_owner,
                }),
            ) {
                Ok(()) => {
                    copy = wait_for_copy_settle(
                        original_sequence,
                        Instant::now() + COPY_ATTEMPT_TIMEOUT,
                    );
                }
                Err(CopySendError::MayHaveCopied(error)) => {
                    if let Some(observed) = wait_for_copy_settle(
                        original_sequence,
                        Instant::now() + COPY_ATTEMPT_TIMEOUT,
                    ) {
                        restore.observe_copy(observed);
                    }
                    return Err(error);
                }
                Err(CopySendError::NoInput(error)) => return Err(error),
            }
        }
        let copy = copy.ok_or_else(|| {
            crate::runtime_trace::record("clipboard_sequence_timeout");
            ExtractionFailure::EmptyRange
        })?;
        restore.observe_copy(copy);

        let target = read_unicode_text()?;
        let target = selection_core::normalize::normalize_target(&target);
        if target.is_empty() {
            return Err(ExtractionFailure::EmptyRange);
        }
        Ok(TextContext {
            target,
            context: None,
            source: ExtractionSource::Clipboard,
            screen_rect: None,
        })
    }

    struct RestoreClipboard {
        original: Option<ClipboardSnapshot>,
        original_sequence: u32,
        copy_sequence: Option<u32>,
        copy_owner: Option<isize>,
    }

    impl RestoreClipboard {
        fn new(original: ClipboardSnapshot) -> Self {
            let original_sequence = original.sequence;
            Self {
                original: Some(original),
                original_sequence,
                copy_sequence: None,
                copy_owner: None,
            }
        }

        fn observe_copy(&mut self, copy: CopyObservation) {
            if is_sequence_advance(self.original_sequence, copy.sequence) {
                self.copy_sequence = Some(copy.sequence);
                self.copy_owner = copy.owner;
            }
        }
    }

    impl Drop for RestoreClipboard {
        fn drop(&mut self) {
            let Some(copy_sequence) = self.copy_sequence else {
                return;
            };
            // Never overwrite a clipboard update that happened after the
            // synthesized Copy. The owner check catches a later update from
            // another process; a same-process clipboard update remains
            // indistinguishable at this API boundary and is rejected whenever
            // the sequence has changed.
            let current_sequence = unsafe { GetClipboardSequenceNumber() };
            let current_owner = clipboard_owner();
            if should_restore(
                self.original_sequence,
                copy_sequence,
                current_sequence,
                self.copy_owner,
                current_owner,
            ) {
                if let Some(snapshot) = self.original.take() {
                    restore_snapshot(snapshot, copy_sequence, self.copy_owner);
                }
            }
        }
    }

    fn keyboard_input(
        key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
        up: bool,
    ) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                },
            },
        }
    }

    enum CopySendError {
        NoInput(ExtractionFailure),
        MayHaveCopied(ExtractionFailure),
    }

    #[derive(Clone, Copy)]
    struct SourceIdentity {
        process_id: u32,
        root_window: isize,
    }

    #[derive(Clone, Copy)]
    struct ClipboardSignals {
        sequence: u32,
        owner: Option<isize>,
    }

    fn may_have_produced_copy(error: &CopySendError) -> bool {
        matches!(error, CopySendError::MayHaveCopied(_))
    }

    fn send_copy(
        expected_source: SourceIdentity,
        deadline: Instant,
        retry_signals: Option<ClipboardSignals>,
    ) -> Result<(), CopySendError> {
        wait_for_modifier_release(deadline).map_err(CopySendError::NoInput)?;
        let actual_root_window = foreground_root_window();
        let actual_process_id = foreground_process_id();
        // Check immediately before injection as well. This narrows the race
        // in which a hotkey modifier is pressed between polling and SendInput.
        if !modifiers_released()
            || !source_identity_matches(
                expected_source,
                SourceIdentity {
                    process_id: actual_process_id,
                    root_window: actual_root_window,
                },
            )
        {
            if expected_source.root_window != 0 && expected_source.root_window != actual_root_window
            {
                crate::runtime_trace::record("clipboard_root_mismatch");
            }
            return Err(CopySendError::NoInput(ExtractionFailure::PermissionDenied));
        }
        // Revalidate the original clipboard signals inside the retry path,
        // immediately before injection. The earlier guard is only a cheap
        // admission check; this closes the race between that check and
        // SendInput.
        if let Some(ClipboardSignals {
            sequence: original_sequence,
            owner: original_owner,
        }) = retry_signals
        {
            let current_sequence = unsafe { GetClipboardSequenceNumber() };
            let current_owner = clipboard_owner();
            if original_sequence != current_sequence || original_owner != current_owner {
                return Err(CopySendError::NoInput(ExtractionFailure::PermissionDenied));
            }
        }
        let inputs = [
            keyboard_input(VK_LCONTROL, false),
            keyboard_input(VK_C, false),
            keyboard_input(VK_C, true),
            keyboard_input(VK_LCONTROL, true),
        ];
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            crate::runtime_trace::record("clipboard_input_sent");
            Ok(())
        } else {
            release_partial_copy(sent as usize);
            if sent == 0 {
                Err(CopySendError::NoInput(ExtractionFailure::PermissionDenied))
            } else {
                Err(CopySendError::MayHaveCopied(
                    ExtractionFailure::PermissionDenied,
                ))
            }
        }
    }

    fn release_partial_copy(sent: usize) {
        match sent {
            1 => send_keyups(&[keyboard_input(VK_LCONTROL, true)]),
            2 => send_keyups(&[
                keyboard_input(VK_C, true),
                keyboard_input(VK_LCONTROL, true),
            ]),
            3 => send_keyups(&[keyboard_input(VK_LCONTROL, true)]),
            _ => {}
        }
    }

    fn send_keyups(inputs: &[INPUT]) {
        let _ = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    }

    fn wait_for_modifier_release(deadline: Instant) -> Result<(), ExtractionFailure> {
        loop {
            if modifiers_released() {
                return Ok(());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ExtractionFailure::PermissionDenied);
            };
            thread::sleep(remaining.min(WAIT_SLICE));
        }
    }

    fn modifiers_released() -> bool {
        MODIFIER_KEYS
            .iter()
            .all(|key| !is_key_down(unsafe { GetAsyncKeyState(*key) }))
    }

    fn is_key_down(state: i16) -> bool {
        state < 0
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ClipboardStorage {
        HGlobal,
        Unsupported,
    }

    /// Only formats whose clipboard handle is specified to be an HGLOBAL are
    /// cloned. Owner-display, private, and GDI/metafile formats have different
    /// lifetime contracts and are rejected before Copy rather than guessed.
    fn classify_storage(format: u32) -> ClipboardStorage {
        let non_hglobal = [
            CF_BITMAP.0 as u32,
            CF_METAFILEPICT.0 as u32,
            CF_PALETTE.0 as u32,
            CF_ENHMETAFILE.0 as u32,
            CF_OWNERDISPLAY.0 as u32,
            CF_DSPBITMAP.0 as u32,
            CF_DSPMETAFILEPICT.0 as u32,
            CF_DSPENHMETAFILE.0 as u32,
        ];
        if non_hglobal.contains(&format)
            || (CF_PRIVATEFIRST.0 as u32..=CF_PRIVATELAST.0 as u32).contains(&format)
            || (CF_GDIOBJFIRST.0 as u32..=CF_GDIOBJLAST.0 as u32).contains(&format)
        {
            ClipboardStorage::Unsupported
        } else {
            ClipboardStorage::HGlobal
        }
    }

    fn snapshot_budget_allows(format_count: usize, bytes: usize, next_bytes: usize) -> bool {
        format_count < MAX_SNAPSHOT_FORMATS
            && next_bytes > 0
            && bytes
                .checked_add(next_bytes)
                .is_some_and(|total| total <= MAX_SNAPSHOT_BYTES)
    }

    struct OwnedClipboardFormat {
        format: u32,
        handle: HANDLE,
    }

    impl OwnedClipboardFormat {
        fn new(format: u32, handle: HANDLE) -> Self {
            Self { format, handle }
        }

        fn transfer_to_clipboard(&mut self) -> Result<(), ExtractionFailure> {
            if self.handle.is_invalid() {
                return Err(ExtractionFailure::Platform);
            }
            unsafe { SetClipboardData(self.format, Some(self.handle)) }
                .map_err(|_| ExtractionFailure::Platform)?;
            // SetClipboardData transfers ownership to the system.
            self.handle = HANDLE::default();
            Ok(())
        }
    }

    impl Drop for OwnedClipboardFormat {
        fn drop(&mut self) {
            if !self.handle.is_invalid() {
                // Every admitted format is HGLOBAL-backed. Unsupported handle
                // types are rejected before duplication, so GlobalFree is the
                // one correct destructor here.
                let _ = unsafe { GlobalFree(Some(HGLOBAL(self.handle.0))) };
            }
        }
    }

    struct ClipboardSnapshot {
        formats: Vec<OwnedClipboardFormat>,
        sequence: u32,
        owner: Option<isize>,
    }

    fn snapshot_original(deadline: Instant) -> Result<ClipboardSnapshot, ExtractionFailure> {
        loop {
            open_clipboard_until(None, deadline)?;
            let snapshot = snapshot_open_clipboard(deadline);
            let _ = unsafe { CloseClipboard() };

            match snapshot {
                Ok(snapshot) => return Ok(snapshot),
                Err(SnapshotFailure::Changed) => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        return Err(ExtractionFailure::Platform);
                    };
                    thread::sleep(remaining.min(WAIT_SLICE));
                }
                Err(SnapshotFailure::Unsafe) => return Err(ExtractionFailure::Platform),
            }
        }
    }

    enum SnapshotFailure {
        Changed,
        Unsafe,
    }

    fn snapshot_open_clipboard(deadline: Instant) -> Result<ClipboardSnapshot, SnapshotFailure> {
        let before = unsafe { GetClipboardSequenceNumber() };
        let mut formats = Vec::new();
        let mut total_bytes = 0usize;
        let mut current = 0u32;

        loop {
            if deadline.checked_duration_since(Instant::now()).is_none() {
                return Err(SnapshotFailure::Unsafe);
            }
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let next = unsafe { EnumClipboardFormats(current) };
            if next == 0 {
                if unsafe { GetLastError() }.0 != 0 {
                    return Err(SnapshotFailure::Unsafe);
                }
                break;
            }
            if classify_storage(next) != ClipboardStorage::HGlobal {
                return Err(SnapshotFailure::Unsafe);
            }
            let source = unsafe { GetClipboardData(next) }.map_err(|_| SnapshotFailure::Unsafe)?;
            let size = unsafe { GlobalSize(HGLOBAL(source.0)) };
            if !snapshot_budget_allows(formats.len(), total_bytes, size) {
                return Err(SnapshotFailure::Unsafe);
            }
            let duplicate =
                unsafe { OleDuplicateData(source, CLIPBOARD_FORMAT(next as u16), GMEM_MOVEABLE) };
            if duplicate.is_invalid() {
                return Err(SnapshotFailure::Unsafe);
            }
            total_bytes += size;
            formats.push(OwnedClipboardFormat::new(next, duplicate));
            current = next;
        }

        if deadline.checked_duration_since(Instant::now()).is_none() {
            return Err(SnapshotFailure::Unsafe);
        }
        let after = unsafe { GetClipboardSequenceNumber() };
        if before != after {
            return Err(SnapshotFailure::Changed);
        }
        Ok(ClipboardSnapshot {
            formats,
            sequence: after,
            owner: clipboard_owner(),
        })
    }

    fn open_clipboard_until(
        owner: Option<HWND>,
        deadline: Instant,
    ) -> Result<(), ExtractionFailure> {
        loop {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ExtractionFailure::Platform);
            };
            thread::sleep(remaining.min(WAIT_SLICE));
        }
    }

    fn restore_snapshot(
        mut snapshot: ClipboardSnapshot,
        copy_sequence: u32,
        copy_owner: Option<isize>,
    ) {
        // A short-lived message-only window gives EmptyClipboard a valid
        // owner. All formats are supplied immediately, so no render messages
        // remain after the window is destroyed.
        let owner = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap_or_default();
        if owner.0.is_null() {
            return;
        }
        let _owner = WindowGuard(owner);
        let deadline = Instant::now() + RESTORE_TIMEOUT;
        if open_clipboard_until(Some(owner), deadline).is_err() {
            return;
        }
        let _close = CloseClipboardGuard;

        // Revalidate after acquiring exclusive clipboard access. This keeps
        // restoration fail-closed if another app won the race while OpenClipboard
        // was retrying.
        if !should_restore(
            snapshot.sequence,
            copy_sequence,
            unsafe { GetClipboardSequenceNumber() },
            copy_owner,
            clipboard_owner(),
        ) {
            return;
        }
        if unsafe { EmptyClipboard() }.is_err() {
            return;
        }
        // Continue after an individual SetClipboardData failure so every
        // independently cloned original format still has a chance to return.
        for format in &mut snapshot.formats {
            let _ = format.transfer_to_clipboard();
        }
    }

    struct WindowGuard(HWND);

    impl Drop for WindowGuard {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    struct CopyObservation {
        sequence: u32,
        owner: Option<isize>,
    }

    fn wait_for_copy_settle(before: u32, deadline: Instant) -> Option<CopyObservation> {
        let first = wait_for_sequence_change(before, deadline)?;
        let mut current = first;
        let mut stable_deadline = Instant::now()
            .checked_add(COPY_SETTLE)
            .unwrap_or(deadline)
            .min(deadline);
        loop {
            let observed = unsafe { GetClipboardSequenceNumber() };
            if observed != current {
                current = observed;
                stable_deadline = Instant::now()
                    .checked_add(COPY_SETTLE)
                    .unwrap_or(deadline)
                    .min(deadline);
                continue;
            }
            let now = Instant::now();
            if now >= stable_deadline {
                return Some(CopyObservation {
                    sequence: current,
                    owner: clipboard_owner(),
                });
            }
            let remaining = stable_deadline.checked_duration_since(now)?;
            thread::sleep(remaining.min(WAIT_SLICE));
        }
    }

    fn wait_for_sequence_change(before: u32, deadline: Instant) -> Option<u32> {
        loop {
            let current = unsafe { GetClipboardSequenceNumber() };
            if current != before {
                return Some(current);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            thread::sleep(remaining.min(WAIT_SLICE));
        }
    }

    fn is_sequence_advance(before: u32, after: u32) -> bool {
        after != before
    }

    fn clipboard_owner() -> Option<isize> {
        unsafe { GetClipboardOwner() }
            .ok()
            .map(|owner| owner.0 as isize)
            .filter(|owner| *owner != 0)
    }

    fn source_identity_matches(expected: SourceIdentity, actual: SourceIdentity) -> bool {
        if expected.root_window != 0 {
            // Chromium/Electron accessibility elements may belong to a
            // renderer PID. The captured top-level root is the authoritative
            // identity for an automatic Copy attempt.
            expected.root_window == actual.root_window
        } else {
            expected.process_id == 0 || expected.process_id == actual.process_id
        }
    }

    fn foreground_process_id() -> u32 {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return 0;
        }
        let mut process_id = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        }
        process_id
    }

    fn foreground_root_window() -> isize {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0.is_null() {
            return 0;
        }
        let root = unsafe { GetAncestor(foreground, GA_ROOT) };
        if root.0.is_null() {
            foreground.0 as isize
        } else {
            root.0 as isize
        }
    }

    fn foreground_source_identity() -> SourceIdentity {
        SourceIdentity {
            process_id: foreground_process_id(),
            root_window: foreground_root_window(),
        }
    }

    fn copy_retry_allowed(
        expected: SourceIdentity,
        actual: SourceIdentity,
        original: ClipboardSignals,
        current: ClipboardSignals,
    ) -> bool {
        source_identity_matches(expected, actual)
            && original.sequence == current.sequence
            && original.owner == current.owner
    }

    /// The restoration invariant is fail-closed: restore only when the copy
    /// advanced the original sequence, the sequence is still the settled copy
    /// sequence (including a u32 wraparound), and the owner signal is exactly
    /// unchanged. A later sequence or owner change therefore never restores.
    fn should_restore(
        original: u32,
        copy: u32,
        current: u32,
        copy_owner: Option<isize>,
        current_owner: Option<isize>,
    ) -> bool {
        is_sequence_advance(original, copy)
            && current == copy
            // An unavailable owner is not a wildcard. Both signals must be
            // equal: the same known owner, or both unknown. This fails closed
            // if the owner changes even when the sequence API is inconclusive.
            && copy_owner == current_owner
    }

    fn clipboard_fallback_allowed(trigger: TriggerKind) -> bool {
        matches!(trigger, TriggerKind::Manual | TriggerKind::Selection)
    }

    fn read_unicode_text() -> Result<String, ExtractionFailure> {
        unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }
            .map_err(|_| ExtractionFailure::EmptyRange)?;
        unsafe { OpenClipboard(None) }.map_err(|_| ExtractionFailure::Platform)?;
        let _close = CloseClipboardGuard;
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }
            .map_err(|_| ExtractionFailure::EmptyRange)?;
        let memory = HGLOBAL(handle.0);
        let bytes = unsafe { GlobalSize(memory) };
        if bytes < std::mem::size_of::<u16>() {
            return Err(ExtractionFailure::EmptyRange);
        }
        let pointer = unsafe { GlobalLock(memory) }.cast::<u16>();
        if pointer.is_null() {
            return Err(ExtractionFailure::Platform);
        }
        let _unlock = UnlockGuard(memory);
        let units = (bytes / std::mem::size_of::<u16>()).min(MAX_CLIPBOARD_UNITS);
        let slice = unsafe { std::slice::from_raw_parts(pointer, units) };
        let length = slice.iter().position(|unit| *unit == 0).unwrap_or(units);
        String::from_utf16(&slice[..length]).map_err(|_| ExtractionFailure::Platform)
    }

    struct CloseClipboardGuard;
    impl Drop for CloseClipboardGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    struct UnlockGuard(HGLOBAL);
    impl Drop for UnlockGuard {
        fn drop(&mut self) {
            let _ = unsafe { GlobalUnlock(self.0) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stable_single_or_multi_copy_sequence_is_restorable() {
            assert!(should_restore(41, 42, 42, Some(7), Some(7)));
            assert!(!should_restore(41, 42, 43, Some(7), Some(7)));
            assert!(should_restore(41, 43, 43, Some(7), Some(7)));
        }

        #[test]
        fn owner_signal_must_remain_compatible() {
            assert!(should_restore(41, 43, 43, Some(7), Some(7)));
            assert!(!should_restore(41, 43, 43, Some(7), Some(8)));
            assert!(!should_restore(41, 43, 43, Some(7), None));
            assert!(!should_restore(41, 43, 43, None, Some(9)));
            assert!(should_restore(41, 43, 43, None, None));
        }

        #[test]
        fn sequence_wraparound_is_an_advance_but_later_sequence_is_not_restored() {
            assert!(should_restore(u32::MAX, 0, 0, Some(7), Some(7)));
            assert!(!should_restore(u32::MAX, 0, 1, Some(7), Some(7)));
        }

        #[test]
        fn newer_sequence_never_restores_even_when_owner_is_same() {
            assert!(!should_restore(41, 43, 44, Some(7), Some(7)));
        }

        #[test]
        fn modifier_state_uses_high_bit_only() {
            assert!(is_key_down(i16::MIN));
            assert!(is_key_down(-1));
            assert!(!is_key_down(0));
            assert!(!is_key_down(i16::MAX));
        }

        #[test]
        fn clipboard_wait_is_bounded_by_the_shared_timeout() {
            assert_eq!(SNAPSHOT_TIMEOUT, Duration::from_millis(250));
            assert!(INPUT_TIMEOUT < COPY_ATTEMPT_TIMEOUT);
            assert!(WAIT_SLICE <= COPY_ATTEMPT_TIMEOUT);
            assert!(
                REQUEST_TIMEOUT.as_millis()
                    > SNAPSHOT_TIMEOUT.as_millis()
                        + INPUT_TIMEOUT.as_millis()
                        + (COPY_ATTEMPT_TIMEOUT.as_millis() * 2)
                        + RESTORE_TIMEOUT.as_millis()
            );
        }

        #[test]
        fn copy_retry_requires_same_root_process_and_original_clipboard_signals() {
            assert!(copy_retry_allowed(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
            ));
            assert!(copy_retry_allowed(
                SourceIdentity {
                    process_id: 0,
                    root_window: 0
                },
                SourceIdentity {
                    process_id: 99,
                    root_window: 777
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: None
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: None
                },
            ));
            assert!(!copy_retry_allowed(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 99,
                    root_window: 101
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
            ));
            assert!(!copy_retry_allowed(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 101
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
            ));
            assert!(!copy_retry_allowed(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
                ClipboardSignals {
                    sequence: 11,
                    owner: Some(7)
                },
            ));
            assert!(!copy_retry_allowed(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(8)
                },
            ));
            assert!(!copy_retry_allowed(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: Some(7)
                },
                ClipboardSignals {
                    sequence: 10,
                    owner: None
                },
            ));
        }

        #[test]
        fn root_guard_is_primary_and_zero_is_only_generic_wildcard() {
            assert!(source_identity_matches(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 99,
                    root_window: 100
                },
            ));
            assert!(!source_identity_matches(
                SourceIdentity {
                    process_id: 42,
                    root_window: 100
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 101
                },
            ));
            assert!(source_identity_matches(
                SourceIdentity {
                    process_id: 42,
                    root_window: 0
                },
                SourceIdentity {
                    process_id: 42,
                    root_window: 99
                },
            ));
            assert!(!source_identity_matches(
                SourceIdentity {
                    process_id: 42,
                    root_window: 0
                },
                SourceIdentity {
                    process_id: 99,
                    root_window: 99
                },
            ));
            assert!(source_identity_matches(
                SourceIdentity {
                    process_id: 0,
                    root_window: 0
                },
                SourceIdentity {
                    process_id: 99,
                    root_window: 99
                },
            ));
        }

        #[test]
        fn no_input_failure_never_qualifies_for_clipboard_observation() {
            assert!(!may_have_produced_copy(&CopySendError::NoInput(
                ExtractionFailure::PermissionDenied,
            )));
            assert!(may_have_produced_copy(&CopySendError::MayHaveCopied(
                ExtractionFailure::PermissionDenied,
            )));
        }

        #[test]
        fn clipboard_fallback_allows_manual_and_selection_but_not_hover() {
            assert!(clipboard_fallback_allowed(TriggerKind::Selection));
            assert!(clipboard_fallback_allowed(TriggerKind::Manual));
            assert!(!clipboard_fallback_allowed(TriggerKind::Hover));
        }

        #[test]
        fn request_mailbox_admits_first_and_rejects_second_until_drained() {
            let (requests, incoming) = request_mailbox();
            let (first_response, _first_result) = mpsc::sync_channel(1);
            let (second_response, _second_result) = mpsc::sync_channel(1);

            requests
                .try_send(Request::Extract {
                    expected_process_id: 0,
                    expected_root_window: 0,
                    response: first_response,
                })
                .expect("the first clipboard request must be admitted");
            assert!(matches!(
                requests.try_send(Request::Extract {
                    expected_process_id: 0,
                    expected_root_window: 0,
                    response: second_response,
                }),
                Err(TrySendError::Full(_))
            ));

            let _ = incoming.recv().expect("the first request remains queued");
            let (third_response, _third_result) = mpsc::sync_channel(1);
            requests
                .try_send(Request::Extract {
                    expected_process_id: 0,
                    expected_root_window: 0,
                    response: third_response,
                })
                .expect("the slot must be reusable after the first request is received");
        }

        #[test]
        fn clipboard_storage_classification_is_fail_closed() {
            assert_eq!(
                classify_storage(CF_UNICODETEXT.0 as u32),
                ClipboardStorage::HGlobal
            );
            assert_eq!(classify_storage(0xc001), ClipboardStorage::HGlobal);

            for format in [
                CF_BITMAP.0 as u32,
                CF_METAFILEPICT.0 as u32,
                CF_PALETTE.0 as u32,
                CF_ENHMETAFILE.0 as u32,
                CF_OWNERDISPLAY.0 as u32,
                CF_DSPBITMAP.0 as u32,
                CF_DSPMETAFILEPICT.0 as u32,
                CF_DSPENHMETAFILE.0 as u32,
                CF_PRIVATEFIRST.0 as u32,
                CF_PRIVATELAST.0 as u32,
                CF_GDIOBJFIRST.0 as u32,
                CF_GDIOBJLAST.0 as u32,
            ] {
                assert_eq!(classify_storage(format), ClipboardStorage::Unsupported);
            }
        }

        #[test]
        fn clipboard_snapshot_budget_is_bounded_and_overflow_safe() {
            assert!(snapshot_budget_allows(0, 0, 1));
            assert!(snapshot_budget_allows(
                MAX_SNAPSHOT_FORMATS - 1,
                0,
                MAX_SNAPSHOT_BYTES
            ));
            assert!(!snapshot_budget_allows(MAX_SNAPSHOT_FORMATS, 0, 1));
            assert!(!snapshot_budget_allows(0, 0, 0));
            assert!(!snapshot_budget_allows(0, MAX_SNAPSHOT_BYTES, 1));
            assert!(!snapshot_budget_allows(0, usize::MAX, 2));
        }
    }
}

#[cfg(windows)]
pub use windows_impl::ClipboardExtractor;

#[cfg(not(windows))]
pub struct ClipboardExtractor;

#[cfg(not(windows))]
impl ClipboardExtractor {
    pub fn new() -> Result<Self, ExtractionFailure> {
        Err(ExtractionFailure::Platform)
    }
}

#[cfg(not(windows))]
impl TextExtractor for ClipboardExtractor {
    fn extract(
        &self,
        _trigger: TriggerKind,
        _pointer: Option<ScreenPoint>,
        _selection_rect: Option<ScreenRect>,
    ) -> ExtractionResult {
        Err(ExtractionFailure::Platform)
    }
}
