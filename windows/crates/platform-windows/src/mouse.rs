//! Event-driven mouse trigger input.
//!
//! `MouseState` is platform-independent and owns the two one-shot deadlines;
//! the Windows part installs `WH_MOUSE_LL` and lets the resident window drive
//! those deadlines with `SetTimer`.  No polling thread is used.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use selection_core::ScreenRect;
pub use selection_platform_interface::ScreenPoint;

pub const SELECTION_DELAY: Duration = Duration::from_millis(80);
pub const HOVER_DELAY: Duration = Duration::from_millis(500);
pub const HOVER_MOVE_THRESHOLD: i32 = 4;
pub const DOUBLE_CLICK_HALF_SIZE: i32 = 8;
pub const DOUBLE_CLICK_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseTrigger {
    Selection {
        pointer: ScreenPoint,
        process_id: u32,
        source_root_window: isize,
        rect: Option<ScreenRect>,
    },
    Hover {
        pointer: ScreenPoint,
        process_id: u32,
        source_root_window: isize,
    },
}

#[derive(Clone, Copy, Debug)]
struct Release {
    pointer: ScreenPoint,
    process_id: u32,
    source_root_window: isize,
    rect: Option<ScreenRect>,
}

/// State for left-button selection and session-only hover mode.
#[derive(Debug, Default)]
pub struct MouseState {
    left_down: Option<(ScreenPoint, u32, isize)>,
    releases: VecDeque<Release>,
    selection_deadline: Option<Instant>,
    hover_enabled: bool,
    /// The point at which the current hover dwell began.  The anchor is
    /// advanced after a significant move instead of comparing every event
    /// with only the immediately preceding event.  This prevents a slow,
    /// continuous one-pixel movement from being mistaken for a stationary
    /// pointer.
    hover_anchor: Option<(ScreenPoint, u32, isize)>,
    hover_candidate: Option<(ScreenPoint, u32, isize)>,
    hover_deadline: Option<Instant>,
    /// Monotonic token for invalidation of an emitted hover candidate.  The
    /// resident compares this around mouse events to cancel extraction that
    /// may already have been queued for the previous point.
    hover_generation: u64,
    last_click: Option<(ScreenPoint, Instant)>,
}

impl MouseState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn hover_enabled(&self) -> bool {
        self.hover_enabled
    }

    pub fn hover_generation(&self) -> u64 {
        self.hover_generation
    }

    pub fn cancel_hover_candidate(&mut self) {
        self.invalidate_hover();
        self.hover_deadline = None;
        self.hover_anchor = None;
        self.hover_candidate = None;
    }

    /// Hover is deliberately not persisted.  A new state always starts off.
    pub fn set_hover_enabled(&mut self, enabled: bool, now: Instant) {
        if self.hover_enabled != enabled {
            self.hover_generation = self.hover_generation.wrapping_add(1);
        }
        self.hover_enabled = enabled;
        self.hover_deadline = None;
        self.hover_anchor = None;
        self.hover_candidate = None;
        if enabled {
            // The next movement establishes the threshold baseline and
            // starts the first dwell interval.
            let _ = now;
        }
    }

    pub fn on_left_down(
        &mut self,
        point: ScreenPoint,
        process_id: u32,
        source_root_window: isize,
        _now: Instant,
    ) {
        self.left_down = Some((point, process_id, source_root_window));
        self.invalidate_hover();
        self.hover_deadline = None;
        self.hover_anchor = None;
        self.hover_candidate = None;
    }

    fn invalidate_hover(&mut self) {
        if self.hover_deadline.is_some()
            || self.hover_anchor.is_some()
            || self.hover_candidate.is_some()
        {
            self.hover_generation = self.hover_generation.wrapping_add(1);
        }
    }

    pub fn on_left_up(
        &mut self,
        point: ScreenPoint,
        process_id: u32,
        source_root_window: isize,
        now: Instant,
    ) {
        let (down, down_pid, down_root) =
            self.left_down
                .take()
                .unwrap_or((point, process_id, source_root_window));
        let dragged = down != point;
        // A drag's originating process belongs to the button-down target.
        // The mouse-up point can be over another window (or an overlay), so
        // preferring its PID breaks extraction of the selection that was just
        // created. If the down event had no process identity, retain the
        // existing up-event fallback.
        let pid = if dragged && down_pid != 0 {
            down_pid
        } else if process_id != 0 {
            process_id
        } else {
            down_pid
        };
        let root = if dragged && down_root != 0 {
            down_root
        } else if source_root_window != 0 {
            source_root_window
        } else {
            down_root
        };
        let double_clicked = self.last_click.is_some_and(|(previous, at)| {
            now.saturating_duration_since(at) <= DOUBLE_CLICK_DELAY
                && (i64::from(previous.x) - i64::from(point.x)).abs()
                    <= i64::from(HOVER_MOVE_THRESHOLD)
                && (i64::from(previous.y) - i64::from(point.y)).abs()
                    <= i64::from(HOVER_MOVE_THRESHOLD)
        });
        let rect = (dragged || double_clicked).then(|| selection_rect(down, point));
        self.last_click = if dragged { None } else { Some((point, now)) };
        self.releases.push_back(Release {
            pointer: point,
            process_id: pid,
            source_root_window: root,
            rect,
        });
        self.selection_deadline = Some(now + SELECTION_DELAY);
        self.invalidate_hover();
        self.hover_deadline = None;
        self.hover_anchor = None;
        self.hover_candidate = None;
    }

    pub fn on_move(
        &mut self,
        point: ScreenPoint,
        process_id: u32,
        source_root_window: isize,
        now: Instant,
    ) {
        if !self.hover_enabled || self.left_down.is_some() {
            return;
        }

        let process_changed = self
            .hover_anchor
            .is_some_and(|(_, anchor_process, anchor_root)| {
                anchor_process != process_id || anchor_root != source_root_window
            });
        let significant_move = self.hover_anchor.is_some_and(|(anchor, _, _)| {
            let dx = i64::from(point.x) - i64::from(anchor.x);
            let dy = i64::from(point.y) - i64::from(anchor.y);
            dx * dx + dy * dy > i64::from(HOVER_MOVE_THRESHOLD) * i64::from(HOVER_MOVE_THRESHOLD)
        });

        // A process change starts a new candidate even when the pointer did
        // not move.  A significant displacement likewise advances the
        // anchor.  Movement inside the Euclidean threshold is jitter: it
        // must not extend the dwell or replace the candidate.
        if self.hover_anchor.is_none() || process_changed || significant_move {
            if process_changed || significant_move {
                self.hover_generation = self.hover_generation.wrapping_add(1);
            }
            self.hover_anchor = Some((point, process_id, source_root_window));
            self.hover_candidate = Some((point, process_id, source_root_window));
            self.hover_deadline = Some(now + HOVER_DELAY);
        }
    }

    pub fn selection_deadline(&self) -> Option<Instant> {
        self.selection_deadline
    }
    pub fn hover_deadline(&self) -> Option<Instant> {
        self.hover_deadline
    }

    /// Emits only timers that have expired.  The resident calls this from the
    /// corresponding one-shot `WM_TIMER`, then arms the next deadline.
    pub fn take_due(&mut self, now: Instant) -> Vec<MouseTrigger> {
        let mut events = Vec::with_capacity(2);
        if self.selection_deadline.is_some_and(|due| due <= now) {
            self.selection_deadline = None;
            if let Some(release) = self.releases.pop_front() {
                events.push(MouseTrigger::Selection {
                    pointer: release.pointer,
                    process_id: release.process_id,
                    source_root_window: release.source_root_window,
                    rect: release.rect,
                });
            }
            if !self.releases.is_empty() {
                self.selection_deadline = Some(now + SELECTION_DELAY);
            }
        }
        if self.hover_deadline.is_some_and(|due| due <= now) {
            self.hover_deadline = None;
            if let Some((pointer, process_id, source_root_window)) = self.hover_candidate.take() {
                events.push(MouseTrigger::Hover {
                    pointer,
                    process_id,
                    source_root_window,
                });
            }
        }
        events
    }
}

pub fn selection_rect(start: ScreenPoint, end: ScreenPoint) -> ScreenRect {
    let mut left = start.x.min(end.x);
    let mut right = start.x.max(end.x);
    let mut top = start.y.min(end.y);
    let mut bottom = start.y.max(end.y);
    if left == right {
        left = left.saturating_sub(DOUBLE_CLICK_HALF_SIZE);
        right = right.saturating_add(DOUBLE_CLICK_HALF_SIZE);
    }
    if top == bottom {
        top = top.saturating_sub(DOUBLE_CLICK_HALF_SIZE);
        bottom = bottom.saturating_add(DOUBLE_CLICK_HALF_SIZE);
    }
    ScreenRect::new(left, top, right, bottom)
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use windows::core::{Error, HRESULT};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostMessageW,
        PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, MSG,
        MSLLHOOKSTRUCT, PM_NOREMOVE, WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
    };

    pub const WM_MOUSE_HOOK: u32 = WM_APP + 20;
    pub const TIMER_SELECTION: usize = 0x7301;
    pub const TIMER_HOVER: usize = 0x7302;

    static HOOK_TARGET: AtomicIsize = AtomicIsize::new(0);
    static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);
    const HOOK_STARTUP_ERROR: HRESULT = HRESULT(0x8000_4005u32 as i32);

    pub(super) fn should_forward_hook_message(kind: u32) -> bool {
        matches!(
            kind,
            WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_MBUTTONDOWN
                | WM_XBUTTONDOWN
        )
    }

    #[derive(Debug)]
    #[repr(C)]
    pub struct RawMouseMessage {
        pub kind: u32,
        pub x: i32,
        pub y: i32,
    }

    pub struct MouseHook {
        thread_id: u32,
        thread: Option<JoinHandle<()>>,
    }

    impl MouseHook {
        pub fn install(hwnd: HWND) -> windows::core::Result<Self> {
            if HOOK_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(Error::new(
                    HOOK_STARTUP_ERROR,
                    "a mouse hook is already installed",
                ));
            }

            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let target = hwnd.0 as isize;
            let thread = match thread::Builder::new()
                .name("selection-translate-mouse-hook".to_owned())
                .spawn(move || hook_thread(target, ready_tx))
            {
                Ok(thread) => thread,
                Err(_error) => {
                    HOOK_ACTIVE.store(false, Ordering::Release);
                    return Err(Error::new(
                        HOOK_STARTUP_ERROR,
                        "the mouse hook worker could not be created",
                    ));
                }
            };
            let (thread_id, failed) = ready_rx.recv().unwrap_or((0, true));
            if failed {
                let _ = thread.join();
                HOOK_TARGET.store(0, Ordering::Release);
                HOOK_ACTIVE.store(false, Ordering::Release);
                return Err(Error::new(
                    HOOK_STARTUP_ERROR,
                    "the mouse hook worker failed to start",
                ));
            }
            Ok(Self {
                thread_id,
                thread: Some(thread),
            })
        }
    }

    impl Drop for MouseHook {
        fn drop(&mut self) {
            // The hook belongs to the worker thread.  Ask its message loop to
            // exit so it can unhook on the same thread, then wait for it.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            HOOK_TARGET.store(0, Ordering::Release);
            HOOK_ACTIVE.store(false, Ordering::Release);
        }
    }

    fn hook_thread(target: isize, ready: mpsc::SyncSender<(u32, bool)>) {
        let thread_id = unsafe { GetCurrentThreadId() };
        // Creating the queue before installing the hook makes posting WM_QUIT
        // from Drop deterministic, even if no hook message has arrived yet.
        let mut queue_probe = MSG::default();
        let _ = unsafe { PeekMessageW(&mut queue_probe, None, 0, 0, PM_NOREMOVE) };
        HOOK_TARGET.store(target, Ordering::Release);
        let instance = match unsafe { GetModuleHandleW(None) } {
            Ok(instance) => instance,
            Err(_error) => {
                let _ = ready.send((thread_id, true));
                HOOK_TARGET.store(0, Ordering::Release);
                return;
            }
        };
        let hook = match unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(low_level_proc),
                Some(HINSTANCE(instance.0)),
                0,
            )
        } {
            Ok(hook) => hook,
            Err(_error) => {
                let _ = ready.send((thread_id, true));
                HOOK_TARGET.store(0, Ordering::Release);
                return;
            }
        };
        if ready.send((thread_id, false)).is_err() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
            HOOK_TARGET.store(0, Ordering::Release);
            return;
        }

        let mut message = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
            if result <= 0 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let _ = unsafe { UnhookWindowsHookEx(hook) };
        HOOK_TARGET.store(0, Ordering::Release);
    }

    unsafe extern "system" fn low_level_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let target = HOOK_TARGET.load(Ordering::Acquire);
            if target != 0 {
                let kind = wparam.0 as u32;
                if should_forward_hook_message(kind) {
                    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                    let event = Box::new(RawMouseMessage {
                        kind,
                        x: info.pt.x,
                        y: info.pt.y,
                    });
                    let raw = Box::into_raw(event) as isize;
                    if PostMessageW(
                        Some(HWND(target as *mut _)),
                        WM_MOUSE_HOOK,
                        WPARAM(0),
                        LPARAM(raw),
                    )
                    .is_err()
                    {
                        drop(Box::from_raw(raw as *mut RawMouseMessage));
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    /// # Safety
    ///
    /// `lparam` must be a pointer produced by `low_level_proc` and posted to
    /// the resident window exactly once. The function takes ownership of it.
    pub unsafe fn take_raw_message(lparam: LPARAM) -> Option<RawMouseMessage> {
        if lparam.0 == 0 {
            return None;
        }
        Some(*Box::from_raw(lparam.0 as *mut RawMouseMessage))
    }

    pub fn hook_error() -> Error {
        Error::from_win32()
    }
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn hook_forwards_every_button_down_used_for_outside_dismissal() {
        use super::windows_impl::should_forward_hook_message;
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
        };

        for message in [
            WM_MOUSEMOVE,
            WM_LBUTTONDOWN,
            WM_RBUTTONDOWN,
            WM_MBUTTONDOWN,
            WM_XBUTTONDOWN,
        ] {
            assert!(should_forward_hook_message(message));
        }
    }

    #[test]
    fn hover_dwell_is_half_a_second() {
        assert_eq!(HOVER_DELAY, Duration::from_millis(500));
    }

    #[test]
    fn release_waits_eighty_ms_and_expands_double_click() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        let p = ScreenPoint::new(100, 200);
        mouse.on_left_down(p, 7, 70, start);
        mouse.on_left_up(p, 7, 70, start);
        assert!(mouse.take_due(start + Duration::from_millis(79)).is_empty());
        assert_eq!(
            mouse.take_due(start + SELECTION_DELAY),
            vec![MouseTrigger::Selection {
                pointer: p,
                process_id: 7,
                source_root_window: 70,
                rect: None
            }]
        );
        mouse.on_left_down(p, 7, 70, start + Duration::from_millis(100));
        mouse.on_left_up(p, 7, 70, start + Duration::from_millis(100));
        assert_eq!(
            mouse.take_due(start + Duration::from_millis(180)),
            vec![MouseTrigger::Selection {
                pointer: p,
                process_id: 7,
                source_root_window: 70,
                rect: Some(ScreenRect::new(92, 192, 108, 208))
            }]
        );
    }

    #[test]
    fn drag_rect_is_normalized_and_hover_uses_a_dwell_after_movement() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.on_left_down(ScreenPoint::new(30, 40), 1, 1, start);
        mouse.on_left_up(ScreenPoint::new(10, 20), 1, 1, start);
        assert_eq!(
            mouse.take_due(start + SELECTION_DELAY),
            vec![MouseTrigger::Selection {
                pointer: ScreenPoint::new(10, 20),
                process_id: 1,
                source_root_window: 1,
                rect: Some(ScreenRect::new(10, 20, 30, 40))
            }]
        );
        mouse.set_hover_enabled(true, start);
        mouse.on_move(ScreenPoint::new(0, 0), 2, 2, start);
        assert_eq!(mouse.hover_deadline(), Some(start + HOVER_DELAY));
        assert!(mouse
            .take_due(start + HOVER_DELAY - Duration::from_millis(1))
            .is_empty());
        assert!(mouse.hover_deadline().is_some());
        assert_eq!(
            mouse.take_due(start + HOVER_DELAY),
            vec![MouseTrigger::Hover {
                pointer: ScreenPoint::new(0, 0),
                process_id: 2,
                source_root_window: 2
            }]
        );
    }

    #[test]
    fn foreground_change_cancels_the_pending_hover_candidate() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);
        mouse.on_move(ScreenPoint::new(12, 18), 2, 20, start);
        let generation = mouse.hover_generation();

        mouse.cancel_hover_candidate();

        assert!(mouse.hover_deadline().is_none());
        assert_ne!(mouse.hover_generation(), generation);
        assert!(mouse.take_due(start + HOVER_DELAY).is_empty());
    }

    #[test]
    fn drag_keeps_the_button_down_process_when_mouse_up_crosses_windows() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.on_left_down(ScreenPoint::new(30, 40), 101, 101, start);
        mouse.on_left_up(ScreenPoint::new(10, 20), 202, 202, start);

        assert_eq!(
            mouse.take_due(start + SELECTION_DELAY),
            vec![MouseTrigger::Selection {
                pointer: ScreenPoint::new(10, 20),
                process_id: 101,
                source_root_window: 101,
                rect: Some(ScreenRect::new(10, 20, 30, 40)),
            }]
        );
    }

    #[test]
    fn drag_uses_mouse_up_process_when_button_down_process_is_unknown() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.on_left_down(ScreenPoint::new(30, 40), 0, 0, start);
        mouse.on_left_up(ScreenPoint::new(10, 20), 202, 202, start);

        assert_eq!(
            mouse.take_due(start + SELECTION_DELAY),
            vec![MouseTrigger::Selection {
                pointer: ScreenPoint::new(10, 20),
                process_id: 202,
                source_root_window: 202,
                rect: Some(ScreenRect::new(10, 20, 30, 40)),
            }]
        );
    }

    #[test]
    fn hover_starts_disabled_and_button_down_cancels_pending_hover() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.on_move(ScreenPoint::new(0, 0), 1, 1, start);
        mouse.on_move(ScreenPoint::new(10, 0), 1, 1, start);
        assert!(mouse.hover_deadline().is_none());
        mouse.set_hover_enabled(true, start);
        mouse.on_move(ScreenPoint::new(0, 0), 1, 1, start);
        assert!(mouse.hover_deadline().is_some());
        mouse.on_left_down(ScreenPoint::new(0, 0), 1, 1, start);
        mouse.on_move(
            ScreenPoint::new(20, 0),
            1,
            1,
            start + Duration::from_millis(10),
        );
        assert!(mouse.hover_deadline().is_none());
        assert!(mouse
            .take_due(start + HOVER_DELAY + Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn accumulated_drift_beyond_threshold_resets_dwell() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);

        mouse.on_move(ScreenPoint::new(0, 0), 7, 7, start);
        let moved_at = start + Duration::from_millis(100);
        // Incremental one-pixel events accumulate against the fixed anchor;
        // the fifth pixel is a significant Euclidean displacement.
        for x in 1..=5 {
            mouse.on_move(
                ScreenPoint::new(x, 0),
                7,
                7,
                moved_at + Duration::from_millis(x as u64),
            );
        }
        let settled_at = moved_at + Duration::from_millis(5);
        assert!(mouse
            .take_due(settled_at + HOVER_DELAY - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            mouse.take_due(settled_at + HOVER_DELAY),
            vec![MouseTrigger::Hover {
                pointer: ScreenPoint::new(5, 0),
                process_id: 7,
                source_root_window: 7
            }]
        );
    }

    #[test]
    fn jitter_within_threshold_does_not_reset_deadline_or_candidate() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);
        let points = [
            ScreenPoint::new(100, 100),
            ScreenPoint::new(102, 101),
            ScreenPoint::new(99, 102),
            ScreenPoint::new(101, 99),
        ];
        mouse.on_move(points[0], 9, 9, start);
        let deadline = start + HOVER_DELAY;
        for (index, point) in points.into_iter().skip(1).enumerate() {
            let at = start + Duration::from_millis(index as u64 * 100);
            mouse.on_move(point, 9, 9, at);
            assert!(mouse.take_due(at).is_empty());
        }
        assert_eq!(
            mouse.take_due(deadline),
            vec![MouseTrigger::Hover {
                pointer: points[0],
                process_id: 9,
                source_root_window: 9
            }]
        );
    }

    #[test]
    fn large_move_advances_anchor_and_invalidates_old_deadline() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);
        mouse.on_move(ScreenPoint::new(0, 0), 3, 3, start);
        mouse.on_move(
            ScreenPoint::new(20, 0),
            3,
            3,
            start + Duration::from_millis(100),
        );

        assert!(mouse.take_due(start + HOVER_DELAY).is_empty());
        assert_eq!(
            mouse.take_due(start + Duration::from_millis(100) + HOVER_DELAY),
            vec![MouseTrigger::Hover {
                pointer: ScreenPoint::new(20, 0),
                process_id: 3,
                source_root_window: 3
            }]
        );
    }

    #[test]
    fn process_change_starts_a_new_hover_candidate() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);
        mouse.on_move(ScreenPoint::new(50, 50), 10, 10, start);
        let changed_at = start + Duration::from_millis(100);
        mouse.on_move(ScreenPoint::new(50, 50), 11, 11, changed_at);

        assert!(mouse.take_due(start + HOVER_DELAY).is_empty());
        assert_eq!(
            mouse.take_due(changed_at + HOVER_DELAY),
            vec![MouseTrigger::Hover {
                pointer: ScreenPoint::new(50, 50),
                process_id: 11,
                source_root_window: 11
            }]
        );
    }

    #[test]
    fn significant_move_after_emission_advances_hover_generation() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);
        mouse.on_move(ScreenPoint::new(0, 0), 1, 1, start);
        let initial = mouse.hover_generation();
        assert!(matches!(
            mouse.take_due(start + HOVER_DELAY).as_slice(),
            [MouseTrigger::Hover { .. }]
        ));
        mouse.on_move(
            ScreenPoint::new(10, 0),
            1,
            1,
            start + HOVER_DELAY + Duration::from_millis(1),
        );
        assert_ne!(mouse.hover_generation(), initial);
    }

    #[test]
    fn left_button_after_emission_advances_hover_generation() {
        let start = Instant::now();
        let mut mouse = MouseState::new();
        mouse.set_hover_enabled(true, start);
        let point = ScreenPoint::new(20, 20);
        mouse.on_move(point, 1, 1, start);
        assert!(matches!(
            mouse.take_due(start + HOVER_DELAY).as_slice(),
            [MouseTrigger::Hover { .. }]
        ));
        let initial = mouse.hover_generation();
        mouse.on_left_down(point, 1, 1, start + HOVER_DELAY);
        assert_ne!(mouse.hover_generation(), initial);
        assert!(mouse.hover_deadline().is_none());
    }
}
