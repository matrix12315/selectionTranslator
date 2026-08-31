//! Resident message-only shell and lifecycle owner.

#[cfg(windows)]
mod windows_impl {
    use super::super::composition::{AppRuntime, Pipeline, PipelineEvent, PIPELINE_EVENT};
    use super::super::mouse::{self, MouseTrigger, ScreenPoint};
    use super::super::{config_reload, foreground, hotkey, popup, runtime_trace, tray};
    use selection_core::{
        cache::{CacheKey, ResultCache},
        default_config_path, Coordinator, JobInput, JobPriority, RequestGate, RequestRejection,
        TextContext, TriggerKind,
    };
    use selection_platform_interface::{canonical_utc_now, CancellationToken, CompletedEntry};
    use std::process::{Child, Command};
    use std::sync::mpsc::Receiver;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{
        GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
        WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW,
        GetAncestor, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, KillTimer,
        MessageBoxW, PostQuitMessage, RegisterClassW, SendMessageTimeoutW, SetTimer,
        SetWindowLongPtrW, TranslateMessage, WindowFromPoint, GA_ROOT, GWLP_USERDATA, HWND_MESSAGE,
        MB_ICONERROR, MB_ICONWARNING, MB_OK, MSG, SEND_MESSAGE_TIMEOUT_FLAGS, SMTO_ABORTIFHUNG,
        SMTO_BLOCK, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONDOWN, WM_MBUTTONDOWN,
        WM_RBUTTONDOWN, WM_TIMER, WM_XBUTTONDOWN, WNDCLASSW,
    };

    const CLASS_NAME: PCWSTR = w!("SelectionTranslateResident");
    const RESIDENT_MUTEX_NAME: PCWSTR = w!("Local\\SelectionTranslate.Resident");
    const RESIDENT_LAUNCH_MUTEX_NAME: PCWSTR =
        w!("Local\\SelectionTranslate.ResidentManagerLaunch");
    const RESIDENT_READY_TIMEOUT: Duration = Duration::from_secs(5);
    const RESIDENT_MESSAGE_TIMEOUT_MS: u32 = 1_500;
    const ACK_READY: usize = 0x5354_5201;
    const ACK_REFRESHED: usize = 0x5354_5202;
    const ACK_REJECTED: usize = 0x5354_52ff;
    const DEFAULT_REST_ENABLED: bool = false;
    const PRIORITIZED_SELECTION_PROFILES: [&str; 3] = [
        "linguist-analysis",
        "code-specialist",
        "concise-explanation",
    ];
    const MAX_VISIBLE_POPUPS: usize = 4;

    /// Private resident notifications. They carry no configuration data or
    /// credentials: the resident reloads from its own local sources and
    /// returns a bounded acknowledgement to the manager.
    pub const CREDENTIAL_REFRESH_MESSAGE: u32 =
        windows::Win32::UI::WindowsAndMessaging::WM_APP + 33;
    pub const CONFIG_REFRESH_MESSAGE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 34;
    const RESIDENT_PING_MESSAGE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 35;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ResidentStartOutcome {
        AlreadyRunning,
        Started,
        Unavailable,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum RefreshOutcome {
        Acknowledged,
        ResidentAbsent,
        Unacknowledged,
        Rejected,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ResidentStartPlan {
        UseReadyResident,
        WaitForStartingResident,
        LaunchSibling,
    }

    fn plan_resident_start(ready: bool, resident_mutex_present: bool) -> ResidentStartPlan {
        if ready {
            ResidentStartPlan::UseReadyResident
        } else if resident_mutex_present {
            ResidentStartPlan::WaitForStartingResident
        } else {
            ResidentStartPlan::LaunchSibling
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ResidentDiagnostic {
        FatalStartup,
        ManualHotkeyUnavailable,
        ResultWindowUnavailable,
    }

    impl ResidentDiagnostic {
        fn message(self) -> &'static str {
            match self {
                Self::FatalStartup => {
                    "Selection Translate could not start. Restart the app; if the problem continues, reinstall this package."
                }
                Self::ManualHotkeyUnavailable => {
                    "Manual translation shortcut Ctrl+Alt+T is already in use. Close the conflicting app, then restart Selection Translate."
                }
                Self::ResultWindowUnavailable => {
                    "The translation result window could not be opened. No provider request was sent."
                }
            }
        }

        fn icon(self) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE {
            match self {
                Self::FatalStartup => MB_ICONERROR,
                Self::ManualHotkeyUnavailable | Self::ResultWindowUnavailable => MB_ICONWARNING,
            }
        }
    }

    struct ResidentInstance(HANDLE);

    impl ResidentInstance {
        fn acquire() -> windows::core::Result<Option<Self>> {
            let handle = unsafe { CreateMutexW(None, true, RESIDENT_MUTEX_NAME)? };
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(handle);
                }
                // A manager may race another manager while both are ensuring
                // the resident is present. The existing instance is the
                // successful outcome; a duplicate must exit silently rather
                // than showing a misleading startup error.
                return Ok(None);
            }
            Ok(Some(Self(handle)))
        }
    }

    impl Drop for ResidentInstance {
        fn drop(&mut self) {
            unsafe {
                let _ = ReleaseMutex(self.0);
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    struct LaunchLock {
        handle: HANDLE,
        owned: bool,
    }

    impl LaunchLock {
        fn acquire(timeout_ms: u32) -> Option<Self> {
            let handle = unsafe { CreateMutexW(None, true, RESIDENT_LAUNCH_MUTEX_NAME).ok()? };
            let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            let owned = if existed {
                matches!(
                    unsafe { WaitForSingleObject(handle, timeout_ms) },
                    WAIT_OBJECT_0 | WAIT_ABANDONED
                )
            } else {
                true
            };
            if !owned {
                unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(handle);
                }
                return None;
            }
            Some(Self { handle, owned })
        }
    }

    impl Drop for LaunchLock {
        fn drop(&mut self) {
            unsafe {
                if self.owned {
                    let _ = ReleaseMutex(self.handle);
                }
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }

    pub struct RuntimeGuard {
        com: bool,
        winrt: bool,
    }

    impl RuntimeGuard {
        pub fn initialize() -> windows::core::Result<Self> {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            }
            if let Err(error) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                unsafe {
                    CoUninitialize();
                }
                return Err(error);
            }
            Ok(Self {
                com: true,
                winrt: true,
            })
        }
    }

    impl Drop for RuntimeGuard {
        fn drop(&mut self) {
            if self.winrt {
                unsafe {
                    RoUninitialize();
                }
            }
            if self.com {
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }

    struct ShellState {
        tray: tray::TrayIcon,
        hotkeys: hotkey::Registrations,
        config_watcher: Option<config_reload::ConfigWatcher>,
        popups: Vec<PopupEntry>,
        next_popup_id: popup::PopupId,
        hover_enabled: bool,
        rest_enabled: bool,
        mouse: mouse::MouseState,
        _mouse_hook: mouse::MouseHook,
        _foreground_hook: foreground::ForegroundHook,
        coordinator: Coordinator,
        runtime: AppRuntime,
        pipeline: Pipeline,
        events: Receiver<PipelineEvent>,
        cache: ResultCache,
        pending: Option<PendingAttempt>,
        pending_profile_choice: Option<PendingProfileChoice>,
        last_request: Option<RequestSpec>,
        active_request: Option<ActiveRequest>,
        provider_cancellation: Option<CancellationToken>,
        stream_output: String,
        popup_job_id: Option<u64>,
        active_popup_id: Option<popup::PopupId>,
        presented_trigger: Option<TriggerKind>,
        popup_guard_root_window: isize,
        popup_anchor: popup::Point,
        last_selection_rect: Option<(selection_core::ScreenRect, isize)>,
        active_prompt_id: Option<String>,
        config_generation: u64,
        popup_failure_reported: bool,
        // Diagnostic-only deduplication for hover deadline traces.
        hover_trace_deadline: Option<Instant>,
    }

    struct PopupEntry {
        id: popup::PopupId,
        popup: popup::Popup,
        last_request: Option<RequestSpec>,
        last_text: Option<TextContext>,
        presented_trigger: Option<TriggerKind>,
        guard_root_window: isize,
        anchor: popup::Point,
        parent: Option<popup::PopupId>,
        created_order: u64,
    }

    #[derive(Clone)]
    struct RequestSpec {
        trigger: TriggerKind,
        process_id: u32,
        source_root_window: isize,
        foreground_guard_root_window: isize,
        pointer: Option<ScreenPoint>,
        selection_rect: Option<selection_core::ScreenRect>,
        prompt_id: String,
        bypass_duplicate_suppression: bool,
        source_popup_id: Option<popup::PopupId>,
        destination_popup_id: Option<popup::PopupId>,
    }

    struct PendingAttempt {
        attempt: u64,
        spec: RequestSpec,
        config_generation: u64,
    }

    struct PendingProfileChoice {
        popup_id: popup::PopupId,
        pending: PendingAttempt,
        text: TextContext,
        anchor: popup::Point,
        profile_ids: Vec<String>,
    }

    struct ActiveRequest {
        process_id: u32,
        text: TextContext,
        prepared: selection_core::PreparedRequest,
        cache_key: CacheKey,
    }

    /// Capability token created only after a native result surface is alive
    /// and initialized for this request. Provider startup consumes it.
    struct ResultSurfaceReady {
        popup_id: popup::PopupId,
        staged_entry: Option<PopupEntry>,
        evict_after_commit: Option<popup::PopupId>,
    }

    struct PopupAdmission {
        popup_id: popup::PopupId,
        created_new: bool,
        staged_entry: Option<PopupEntry>,
        evict_after_commit: Option<popup::PopupId>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ResultSurfaceError {
        ProtectedCapacity,
        Unavailable,
    }

    fn protected_capacity_error(
        force_new: bool,
        popup_count: usize,
        eviction_candidate: Option<popup::PopupId>,
    ) -> Option<ResultSurfaceError> {
        (force_new && popup_count >= MAX_VISIBLE_POPUPS && eviction_candidate.is_none())
            .then_some(ResultSurfaceError::ProtectedCapacity)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CandidateCancellation {
        ExtractionOnly,
        AllInflight,
    }

    fn candidate_cancellation(
        trigger: TriggerKind,
        bypass_duplicate_suppression: bool,
    ) -> CandidateCancellation {
        if trigger == TriggerKind::Manual || bypass_duplicate_suppression {
            CandidateCancellation::AllInflight
        } else {
            CandidateCancellation::ExtractionOnly
        }
    }

    fn commit_after_result_surface<T>(
        surface: Option<ResultSurfaceReady>,
        commit: impl FnOnce() -> T,
    ) -> Option<(ResultSurfaceReady, T)> {
        surface.map(|surface| (surface, commit()))
    }

    fn start_provider_after_result_surface<T>(
        _surface: ResultSurfaceReady,
        start: impl FnOnce() -> T,
    ) -> T {
        start()
    }

    pub fn run(runtime: AppRuntime) -> windows::core::Result<()> {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let _runtime = RuntimeGuard::initialize()?;
        let Some(_instance) = ResidentInstance::acquire()? else {
            return Ok(());
        };
        let instance = unsafe { GetModuleHandleW(None)? };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            return Err(windows::core::Error::from_win32());
        }
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                CLASS_NAME,
                w!("Selection Translate"),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )?
        };
        let tray_icon = match tray::TrayIcon::add(hwnd) {
            Ok(icon) => icon,
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        let hotkeys = hotkey::Registrations::register(hwnd, &runtime.config.hotkeys.cycle_profiles);
        let config_watcher =
            default_config_path().and_then(|path| config_reload::ConfigWatcher::start(path, hwnd));
        let mouse_hook = match mouse::MouseHook::install(hwnd) {
            Ok(hook) => hook,
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        let foreground_hook = match foreground::ForegroundHook::install(hwnd) {
            Ok(hook) => hook,
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        let (pipeline, events) = Pipeline::with_receiver(runtime.provider.clone(), hwnd);
        let mut state = Box::new(ShellState {
            tray: tray_icon,
            hotkeys,
            config_watcher,
            popups: Vec::new(),
            next_popup_id: 1,
            hover_enabled: false,
            rest_enabled: DEFAULT_REST_ENABLED,
            mouse: mouse::MouseState::new(),
            _mouse_hook: mouse_hook,
            _foreground_hook: foreground_hook,
            coordinator: Coordinator::new(),
            runtime,
            pipeline,
            events,
            cache: ResultCache::default(),
            pending: None,
            pending_profile_choice: None,
            last_request: None,
            active_request: None,
            provider_cancellation: None,
            stream_output: String::new(),
            popup_job_id: None,
            active_popup_id: None,
            presented_trigger: None,
            popup_guard_root_window: 0,
            popup_anchor: popup::Point { x: 0, y: 0 },
            last_selection_rect: None,
            active_prompt_id: None,
            config_generation: 1,
            popup_failure_reported: false,
            hover_trace_deadline: None,
        });
        unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                (&mut *state as *mut ShellState) as isize,
            );
        }
        if !state.hotkeys.manual_available() {
            show_diagnostic(ResidentDiagnostic::ManualHotkeyUnavailable);
        }
        let mut message = MSG::default();
        let loop_result = loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if result.0 == -1 {
                break Err(windows::core::Error::from_win32());
            }
            if result.0 == 0 {
                break Ok(());
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        };
        if let Some(token) = state.provider_cancellation.take() {
            token.cancel();
        }
        runtime_trace::record("resident_shutdown");
        let _ = state.coordinator.cancel_active();
        state.hotkeys.unregister(hwnd);
        for entry in &mut state.popups {
            entry.popup.dismiss();
        }
        state.popups.clear();
        drop(state);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        loop_result
    }

    pub fn show_resident_startup_failure() {
        show_diagnostic(ResidentDiagnostic::FatalStartup);
    }

    fn show_diagnostic(diagnostic: ResidentDiagnostic) {
        let text: Vec<u16> = diagnostic
            .message()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                w!("Selection Translate"),
                MB_OK | diagnostic.icon(),
            );
        }
    }

    fn find_resident_window() -> Option<HWND> {
        let resident =
            unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS_NAME, PCWSTR::null()).ok()? };
        (!resident.0.is_null()).then_some(resident)
    }

    fn send_resident_message(message: u32) -> Result<usize, RefreshOutcome> {
        let Some(resident) = find_resident_window() else {
            return Err(RefreshOutcome::ResidentAbsent);
        };
        let mut response = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(
                resident,
                message,
                WPARAM(0),
                LPARAM(0),
                SEND_MESSAGE_TIMEOUT_FLAGS(SMTO_ABORTIFHUNG.0 | SMTO_BLOCK.0),
                RESIDENT_MESSAGE_TIMEOUT_MS,
                Some(&mut response),
            )
        };
        if sent.0 == 0 {
            Err(RefreshOutcome::Unacknowledged)
        } else {
            Ok(response)
        }
    }

    fn decode_refresh_response(response: Result<usize, RefreshOutcome>) -> RefreshOutcome {
        match response {
            Ok(ACK_REFRESHED) => RefreshOutcome::Acknowledged,
            Ok(ACK_REJECTED) => RefreshOutcome::Rejected,
            Ok(_) => RefreshOutcome::Unacknowledged,
            Err(outcome) => outcome,
        }
    }

    /// Ask the running resident to reload configuration from disk and wait
    /// for a bounded acknowledgement. No configuration contents cross IPC.
    pub fn notify_config_changed() -> RefreshOutcome {
        decode_refresh_response(send_resident_message(CONFIG_REFRESH_MESSAGE))
    }

    /// Ask the running resident to reread its credential and wait for a
    /// bounded acknowledgement. No credential contents cross IPC.
    pub fn notify_credentials_changed() -> RefreshOutcome {
        decode_refresh_response(send_resident_message(CREDENTIAL_REFRESH_MESSAGE))
    }

    fn resident_ready() -> bool {
        matches!(send_resident_message(RESIDENT_PING_MESSAGE), Ok(ACK_READY))
    }

    fn resident_mutex_present() -> bool {
        let Ok(handle) = (unsafe { CreateMutexW(None, false, RESIDENT_MUTEX_NAME) }) else {
            // Access denial can mean a resident at a different integrity
            // level. Conservatively avoid launching a competing instance.
            return true;
        };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        existed
    }

    fn wait_for_resident(mut child: Option<&mut Child>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if resident_ready() {
                return true;
            }
            if let Some(process) = child.as_deref_mut() {
                if process.try_wait().ok().flatten().is_some() {
                    return false;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(40));
        }
    }

    /// Ensure the sibling resident is ready when the manager is opened
    /// directly. A short-lived named mutex serializes concurrent managers;
    /// the resident itself remains entirely event-driven.
    pub fn ensure_resident_running() -> ResidentStartOutcome {
        if resident_ready() {
            return ResidentStartOutcome::AlreadyRunning;
        }
        let Some(_launch_lock) = LaunchLock::acquire(RESIDENT_READY_TIMEOUT.as_millis() as u32)
        else {
            return ResidentStartOutcome::Unavailable;
        };
        let plan = plan_resident_start(resident_ready(), resident_mutex_present());
        match plan {
            ResidentStartPlan::UseReadyResident => ResidentStartOutcome::AlreadyRunning,
            ResidentStartPlan::WaitForStartingResident => {
                if wait_for_resident(None, RESIDENT_READY_TIMEOUT) {
                    ResidentStartOutcome::AlreadyRunning
                } else {
                    ResidentStartOutcome::Unavailable
                }
            }
            ResidentStartPlan::LaunchSibling => {
                let Ok(executable) = std::env::current_exe() else {
                    return ResidentStartOutcome::Unavailable;
                };
                let resident = executable
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("selection-translate-resident.exe");
                let Ok(mut child) = Command::new(resident).spawn() else {
                    return ResidentStartOutcome::Unavailable;
                };
                if wait_for_resident(Some(&mut child), RESIDENT_READY_TIMEOUT) {
                    ResidentStartOutcome::Started
                } else {
                    ResidentStartOutcome::Unavailable
                }
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let state = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
            as *mut ShellState;
        match message {
            tray::TRAY_CALLBACK => {
                tray::handle_callback(hwnd, lparam, !state.is_null() && (*state).rest_enabled);
                return LRESULT(0);
            }
            popup::POPUP_DISMISSED => {
                if !state.is_null() {
                    remove_popup(&mut *state, lparam.0 as popup::PopupId, false);
                }
                return LRESULT(0);
            }
            popup::POPUP_RETRY => {
                if !state.is_null() {
                    replay_popup(hwnd, &mut *state, lparam.0 as popup::PopupId, false);
                }
                return LRESULT(0);
            }
            popup::POPUP_PROMPT => {
                if !state.is_null() {
                    replay_popup(hwnd, &mut *state, lparam.0 as popup::PopupId, true);
                }
                return LRESULT(0);
            }
            popup::POPUP_PROFILE_SELECTED => {
                if !state.is_null() {
                    begin_selected_profile(hwnd, &mut *state, lparam.0 as popup::PopupId, wparam.0);
                }
                return LRESULT(0);
            }
            PIPELINE_EVENT => {
                if !state.is_null() {
                    drain_pipeline_events(hwnd, &mut *state);
                }
                return LRESULT(0);
            }
            mouse::WM_MOUSE_HOOK => {
                if !state.is_null() {
                    if let Some(raw) = mouse::take_raw_message(lparam) {
                        handle_mouse_message(hwnd, &mut *state, raw);
                    }
                } else {
                    // The hook is removed before the state is dropped, so this
                    // is only defensive handling for a queued message.
                    unsafe {
                        mouse::take_raw_message(lparam);
                    }
                }
                return LRESULT(0);
            }
            foreground::WM_FOREGROUND_CHANGED => {
                if !state.is_null() {
                    let foreground = HWND(wparam.0 as *mut _);
                    handle_foreground_changed(hwnd, &mut *state, foreground);
                }
                return LRESULT(0);
            }
            config_reload::CONFIG_RELOAD_MESSAGE => {
                if !state.is_null() {
                    drain_config_changes(hwnd, &mut *state);
                }
                return LRESULT(0);
            }
            RESIDENT_PING_MESSAGE => {
                return if state.is_null() {
                    LRESULT(0)
                } else {
                    LRESULT(ACK_READY as isize)
                };
            }
            CONFIG_REFRESH_MESSAGE => {
                return if state.is_null() {
                    LRESULT(0)
                } else if reload_config_from_disk(hwnd, &mut *state) {
                    LRESULT(ACK_REFRESHED as isize)
                } else {
                    LRESULT(ACK_REJECTED as isize)
                };
            }
            CREDENTIAL_REFRESH_MESSAGE => {
                return if state.is_null() {
                    LRESULT(0)
                } else {
                    refresh_provider(&mut *state);
                    LRESULT(ACK_REFRESHED as isize)
                };
            }
            WM_TIMER => {
                if !state.is_null() {
                    if (*state).rest_enabled {
                        let _ = KillTimer(Some(hwnd), mouse::TIMER_SELECTION);
                        let _ = KillTimer(Some(hwnd), mouse::TIMER_HOVER);
                        return LRESULT(0);
                    }
                    let now = Instant::now();
                    let events = (*state).mouse.take_due(now);
                    for event in events {
                        if matches!(event, MouseTrigger::Hover { .. }) {
                            runtime_trace::record("hover_timer_emitted");
                        }
                        observe_trigger(hwnd, &mut *state, event, now);
                    }
                    arm_mouse_timers(hwnd, &mut *state, now);
                }
                return LRESULT(0);
            }
            WM_COMMAND => {
                match wparam.0 & 0xffff {
                    tray::OPEN_MANAGER_COMMAND => {
                        if let Err(error) = open_manager() {
                            eprintln!("could not open manager: {error}");
                        }
                    }
                    tray::TOGGLE_HOVER_COMMAND => {
                        if !state.is_null() && !(*state).rest_enabled {
                            (*state).hover_enabled = !(*state).hover_enabled;
                            let enabled = (*state).hover_enabled;
                            if !enabled {
                                cancel_hover_work(&mut *state);
                            }
                            runtime_trace::record(if enabled {
                                "hover_enabled"
                            } else {
                                "hover_disabled"
                            });
                            (*state).mouse.set_hover_enabled(enabled, Instant::now());
                            (*state).hover_trace_deadline = None;
                            (*state).tray.update_status(enabled, false);
                            arm_mouse_timers(hwnd, &mut *state, Instant::now());
                        }
                    }
                    tray::TOGGLE_REST_COMMAND => {
                        if !state.is_null() {
                            let enabled = !(*state).rest_enabled;
                            set_rest_mode(hwnd, &mut *state, enabled);
                        }
                    }
                    tray::EXIT_COMMAND => PostQuitMessage(0),
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_HOTKEY => {
                if !state.is_null() {
                    if (*state).rest_enabled {
                        return LRESULT(0);
                    }
                    match wparam.0 as i32 {
                        hotkey::TOGGLE_POPUP_ID => {
                            observe_manual(hwnd, &mut *state, Instant::now());
                        }
                        hotkey::TOGGLE_HOVER_ID => {
                            (*state).hover_enabled = !(*state).hover_enabled;
                            let enabled = (*state).hover_enabled;
                            if !enabled {
                                cancel_hover_work(&mut *state);
                            }
                            runtime_trace::record(if enabled {
                                "hover_enabled"
                            } else {
                                "hover_disabled"
                            });
                            (*state).mouse.set_hover_enabled(enabled, Instant::now());
                            (*state).hover_trace_deadline = None;
                            (*state).tray.update_status(enabled, false);
                            arm_mouse_timers(hwnd, &mut *state, Instant::now());
                        }
                        hotkey::CYCLE_PROFILES_ID => {
                            cycle_prompt(&mut *state);
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                }
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    fn cursor_position() -> windows::core::Result<popup::Point> {
        let mut point = windows::Win32::Foundation::POINT::default();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point)?;
        }
        Ok(popup::Point {
            x: point.x,
            y: point.y,
        })
    }

    fn process_id_at(point: ScreenPoint) -> u32 {
        let hwnd = unsafe {
            WindowFromPoint(windows::Win32::Foundation::POINT {
                x: point.x,
                y: point.y,
            })
        };
        if hwnd.0.is_null() {
            return 0;
        }
        let mut process_id = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        }
        process_id
    }

    fn root_window(hwnd: HWND) -> HWND {
        if hwnd.0.is_null() {
            return HWND::default();
        }
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        if root.0.is_null() {
            hwnd
        } else {
            root
        }
    }

    fn root_window_at(point: ScreenPoint) -> isize {
        let hwnd = unsafe {
            WindowFromPoint(windows::Win32::Foundation::POINT {
                x: point.x,
                y: point.y,
            })
        };
        root_window(hwnd).0 as isize
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
        root_window(unsafe { GetForegroundWindow() }).0 as isize
    }

    fn popup_entry(state: &ShellState, id: popup::PopupId) -> Option<&PopupEntry> {
        state.popups.iter().find(|entry| entry.id == id)
    }

    fn popup_entry_mut(state: &mut ShellState, id: popup::PopupId) -> Option<&mut PopupEntry> {
        state.popups.iter_mut().find(|entry| entry.id == id)
    }

    fn allocate_popup_id(state: &mut ShellState) -> popup::PopupId {
        let existing: Vec<_> = state.popups.iter().map(|entry| entry.id).collect();
        let (id, next_id) = next_unused_popup_id(state.next_popup_id, &existing);
        state.next_popup_id = next_id;
        id
    }

    fn next_unused_popup_id(
        next_id: popup::PopupId,
        existing: &[popup::PopupId],
    ) -> (popup::PopupId, popup::PopupId) {
        let mut next_id = next_id;
        loop {
            let id = next_id.max(1);
            next_id = id.wrapping_add(1).max(1);
            if !existing.contains(&id) {
                return (id, next_id);
            }
        }
    }

    fn remove_popup(state: &mut ShellState, id: popup::PopupId, dismiss_native: bool) {
        if state.pending.as_ref().is_some_and(|pending| {
            pending.spec.source_popup_id == Some(id)
                || pending.spec.destination_popup_id == Some(id)
        }) {
            cancel_pending_extraction(state);
        }
        if state
            .pending_profile_choice
            .as_ref()
            .is_some_and(|choice| choice.popup_id == id)
        {
            state.pending_profile_choice = None;
        }
        if state.active_popup_id == Some(id) {
            if let Some(token) = state.provider_cancellation.take() {
                token.cancel();
            }
            let _ = state.coordinator.cancel_active();
            state.active_request = None;
            state.active_popup_id = None;
            state.popup_job_id = None;
            state.stream_output.clear();
        }
        if let Some(index) = state.popups.iter().position(|entry| entry.id == id) {
            let mut entry = state.popups.remove(index);
            if dismiss_native {
                entry.popup.dismiss();
            }
        }
        for child in &mut state.popups {
            if child.parent == Some(id) {
                child.parent = None;
            }
        }
        if state.last_request.as_ref().is_some_and(|request| {
            request.destination_popup_id == Some(id) || request.source_popup_id == Some(id)
        }) {
            state.last_request = None;
        }
    }

    fn dismiss_unpinned_outside(state: &mut ShellState, clicked: Option<popup::PopupId>) {
        let ids: Vec<_> = state
            .popups
            .iter()
            .filter(|entry| {
                should_dismiss_for_outside_click(clicked, entry.id, entry.popup.is_pinned())
            })
            .map(|entry| entry.id)
            .collect();
        for id in ids {
            remove_popup(state, id, true);
        }
    }

    fn should_dismiss_for_outside_click(
        clicked: Option<popup::PopupId>,
        candidate: popup::PopupId,
        pinned: bool,
    ) -> bool {
        Some(candidate) != clicked && !pinned
    }

    fn popup_can_be_reused(pinned: bool) -> bool {
        !pinned
    }

    fn popup_can_be_evicted(
        id: popup::PopupId,
        source: Option<popup::PopupId>,
        active_destination: Option<popup::PopupId>,
        pinned: bool,
        completed: bool,
    ) -> bool {
        Some(id) != source && Some(id) != active_destination && !pinned && completed
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PopupEvictionCandidate {
        id: popup::PopupId,
        created_order: u64,
        pinned: bool,
        completed: bool,
    }

    /// Select the oldest popup that can be safely evicted when the visible
    /// popup registry is full. Keeping this policy pure makes it possible to
    /// prove the capacity boundary without creating native windows in tests.
    fn oldest_evictable_popup(
        candidates: &[PopupEvictionCandidate],
        parent: Option<popup::PopupId>,
        active_destination: Option<popup::PopupId>,
    ) -> Option<popup::PopupId> {
        candidates
            .iter()
            .filter(|candidate| {
                popup_can_be_evicted(
                    candidate.id,
                    parent,
                    active_destination,
                    candidate.pinned,
                    candidate.completed,
                )
            })
            .min_by_key(|candidate| candidate.created_order)
            .map(|candidate| candidate.id)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CascadeSourceStatus {
        Missing,
        Unavailable,
        Ready,
    }

    /// A cascade may only proceed when its source popup is still a live,
    /// completed result surface with a usable anchor. This pure gate must run
    /// before RequestGate/coordinator/provider admission.
    fn cascade_source_status(
        present: bool,
        result_surface_available: bool,
        completed: bool,
        anchor_available: bool,
    ) -> CascadeSourceStatus {
        if !present {
            CascadeSourceStatus::Missing
        } else if !result_surface_available || !completed || !anchor_available {
            CascadeSourceStatus::Unavailable
        } else {
            CascadeSourceStatus::Ready
        }
    }

    fn should_dismiss_hover_for_foreground(
        presented_trigger: Option<TriggerKind>,
        guard_root_window: isize,
        new_root_window: isize,
        popup_present: bool,
        popup_pinned: bool,
        foreground_is_popup: bool,
    ) -> bool {
        popup_present
            && !popup_pinned
            && !foreground_is_popup
            && presented_trigger == Some(TriggerKind::Hover)
            && guard_root_window != 0
            && new_root_window != 0
            && guard_root_window != new_root_window
    }

    fn handle_foreground_changed(hwnd: HWND, state: &mut ShellState, foreground: HWND) {
        let foreground_is_popup = state
            .popups
            .iter()
            .any(|entry| entry.popup.owns_window(foreground));
        let new_root_window = root_window(foreground).0 as isize;
        if !foreground_is_popup {
            state.mouse.cancel_hover_candidate();
            cancel_hover_extraction(state);
            state.hover_trace_deadline = None;
            unsafe {
                let _ = KillTimer(Some(hwnd), mouse::TIMER_HOVER);
            }
        }
        let ids: Vec<_> = state
            .popups
            .iter()
            .filter(|entry| {
                should_dismiss_hover_for_foreground(
                    entry.presented_trigger,
                    entry.guard_root_window,
                    new_root_window,
                    true,
                    entry.popup.is_pinned(),
                    foreground_is_popup,
                )
            })
            .map(|entry| entry.id)
            .collect();
        if !ids.is_empty() {
            runtime_trace::record("hover_popup_foreground_changed");
        }
        for id in ids {
            remove_popup(state, id, true);
        }
    }

    fn hover_foreground_guard_is_current(
        trigger: TriggerKind,
        guard_root_window: isize,
        current_root_window: isize,
        foreground_is_popup: bool,
    ) -> bool {
        trigger != TriggerKind::Hover
            || foreground_is_popup
            || (guard_root_window != 0 && guard_root_window == current_root_window)
    }

    fn handle_mouse_message(hwnd: HWND, state: &mut ShellState, raw: mouse::RawMouseMessage) {
        if !translation_enabled(state.rest_enabled) {
            return;
        }
        let now = Instant::now();
        let point = ScreenPoint::new(raw.x, raw.y);
        let process_id = process_id_at(point);
        let source_root_window = root_window_at(point);
        let resident_process_id = std::process::id();
        let native_point = popup::Point {
            x: point.x,
            y: point.y,
        };
        let clicked_popup = state
            .popups
            .iter()
            .rev()
            .find(|entry| entry.popup.contains_window_point(native_point))
            .map(|entry| entry.id);
        let button_down = is_pointer_button_down(raw.kind);
        if button_down {
            dismiss_unpinned_outside(state, clicked_popup);
            runtime_trace::record("popup_outside_click_dismiss");
        }
        // Popup controls keep their native click behavior. Only completed
        // result output participates in recursive Hover extraction.
        let popup_text_hover = raw.kind == windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE
            && state.hover_enabled
            && state
                .popups
                .iter()
                .any(|entry| entry.popup.contains_completed_output_point(native_point));
        if process_id == resident_process_id && !popup_text_hover {
            return;
        }
        if button_down && raw.kind != WM_LBUTTONDOWN {
            state.mouse.cancel_hover_candidate();
            cancel_hover_extraction(state);
            state.hover_trace_deadline = None;
            unsafe {
                let _ = KillTimer(Some(hwnd), mouse::TIMER_HOVER);
            }
            return;
        }
        let before_hover_deadline = state.mouse.hover_deadline();
        let before_hover_generation = state.mouse.hover_generation();
        match raw.kind {
            windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN => {
                state
                    .mouse
                    .on_left_down(point, process_id, source_root_window, now)
            }
            windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP => {
                state
                    .mouse
                    .on_left_up(point, process_id, source_root_window, now)
            }
            windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE => {
                state
                    .mouse
                    .on_move(point, process_id, source_root_window, now);
            }
            _ => return,
        }
        if state.mouse.hover_generation() != before_hover_generation {
            // Moving to a new point invalidates only extraction for the old
            // coordinate. An admitted Hover request/result remains readable
            // until a later valid result replaces it or foreground lifetime
            // handling explicitly dismisses it.
            cancel_hover_extraction(state);
        }
        if raw.kind == windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE
            && state.hover_enabled
            && state.mouse.hover_deadline() != before_hover_deadline
        {
            runtime_trace::record("hover_mouse_move_accepted");
        }
        arm_mouse_timers(hwnd, state, now);
    }

    fn is_pointer_button_down(kind: u32) -> bool {
        matches!(
            kind,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
    }

    fn arm_mouse_timers(hwnd: HWND, state: &mut ShellState, now: Instant) {
        let selection = state.mouse.selection_deadline();
        let hover = state.mouse.hover_deadline();
        if hover != state.hover_trace_deadline {
            if hover.is_some() {
                runtime_trace::record("hover_deadline_armed");
            }
            state.hover_trace_deadline = hover;
        }
        unsafe {
            if let Some(deadline) = selection {
                let millis = deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .clamp(1, u128::from(u32::MAX)) as u32;
                let _ = SetTimer(Some(hwnd), mouse::TIMER_SELECTION, millis, None);
            } else {
                let _ = KillTimer(Some(hwnd), mouse::TIMER_SELECTION);
            }
            if let Some(deadline) = hover {
                let millis = deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .clamp(1, u128::from(u32::MAX)) as u32;
                let _ = SetTimer(Some(hwnd), mouse::TIMER_HOVER, millis, None);
            } else {
                let _ = KillTimer(Some(hwnd), mouse::TIMER_HOVER);
            }
        }
    }

    fn observe_trigger(hwnd: HWND, state: &mut ShellState, trigger: MouseTrigger, now: Instant) {
        if !translation_enabled(state.rest_enabled) {
            return;
        }
        let (kind, point, process_id, source_root_window, rect) = match trigger {
            MouseTrigger::Selection {
                pointer,
                process_id,
                source_root_window,
                rect,
            } => (
                TriggerKind::Selection,
                pointer,
                process_id,
                source_root_window,
                rect,
            ),
            MouseTrigger::Hover {
                pointer,
                process_id,
                source_root_window,
            } => (
                TriggerKind::Hover,
                pointer,
                process_id,
                source_root_window,
                None,
            ),
        };
        if kind == TriggerKind::Selection {
            state.last_selection_rect = rect.map(|rect| (rect, source_root_window));
        }
        let prompt_id = selected_prompt_id(state, kind);
        let source_popup_id = (kind == TriggerKind::Hover)
            .then(|| {
                state.popups.iter().rev().find_map(|entry| {
                    entry
                        .popup
                        .contains_completed_output_point(popup::Point {
                            x: point.x,
                            y: point.y,
                        })
                        .then_some(entry.id)
                })
            })
            .flatten();
        let foreground_guard_root_window = source_popup_id
            .and_then(|id| popup_entry(state, id).map(|entry| entry.guard_root_window))
            .unwrap_or_else(foreground_root_window);
        begin_request(
            hwnd,
            state,
            RequestSpec {
                trigger: kind,
                process_id,
                source_root_window,
                foreground_guard_root_window,
                pointer: Some(point),
                selection_rect: rect,
                prompt_id,
                bypass_duplicate_suppression: false,
                source_popup_id,
                destination_popup_id: None,
            },
            true,
            now,
        );
    }

    fn observe_manual(hwnd: HWND, state: &mut ShellState, now: Instant) {
        if !translation_enabled(state.rest_enabled) {
            return;
        }
        let pointer = cursor_position()
            .ok()
            .map(|point| ScreenPoint::new(point.x, point.y));
        // Manual is a hotkey action. The source is the foreground application
        // receiving the hotkey, not whichever window happens to be beneath
        // the pointer when the request is assembled.
        let process_id = foreground_process_id();
        let source_root_window = foreground_root_window();
        begin_request(
            hwnd,
            state,
            RequestSpec {
                trigger: TriggerKind::Manual,
                process_id,
                source_root_window,
                foreground_guard_root_window: source_root_window,
                pointer,
                selection_rect: selection_rect_for_root(
                    state.last_selection_rect,
                    source_root_window,
                ),
                prompt_id: selected_prompt_id(state, TriggerKind::Manual),
                bypass_duplicate_suppression: false,
                source_popup_id: None,
                destination_popup_id: None,
            },
            true,
            now,
        );
    }

    fn selection_rect_for_root(
        remembered: Option<(selection_core::ScreenRect, isize)>,
        current_root: isize,
    ) -> Option<selection_core::ScreenRect> {
        remembered.and_then(|(rect, root)| (root != 0 && root == current_root).then_some(rect))
    }

    fn begin_request(
        hwnd: HWND,
        state: &mut ShellState,
        spec: RequestSpec,
        reset_prompt: bool,
        now: Instant,
    ) {
        if !translation_enabled(state.rest_enabled) {
            return;
        }
        let mut spec = spec;
        if let Some(error) = state.runtime.startup_error.clone() {
            if spec.trigger == TriggerKind::Manual {
                let anchor = spec
                    .pointer
                    .map(|point| popup::Point {
                        x: point.x,
                        y: point.y,
                    })
                    .or_else(|| cursor_position().ok())
                    .unwrap_or(popup::Point { x: 0, y: 0 });
                show_local_error(hwnd, state, 0, anchor, &error);
            }
            return;
        }
        if state.pending_profile_choice.is_some() {
            if spec.trigger == TriggerKind::Hover {
                return;
            }
            if let Some(id) = state
                .pending_profile_choice
                .as_ref()
                .map(|choice| choice.popup_id)
            {
                remove_popup(state, id, true);
            }
        }
        if reset_prompt {
            spec.prompt_id = selected_prompt_id(state, spec.trigger);
        }
        let priority = JobPriority::from(spec.trigger);
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| JobPriority::from(pending.spec.trigger) > priority)
        {
            return;
        }
        if state
            .coordinator
            .active()
            .is_some_and(|active| active.priority > priority)
        {
            return;
        }
        // Automatic extraction is only a candidate replacement. Keep a slow
        // provider request and its loading/result popup authoritative until
        // this candidate yields valid text and passes the request gate. A
        // click with no text must not cancel a delayed Selection response.
        // Manual/Retry/Prompt are explicit replacement commands and retain
        // their immediate-cancellation semantics.
        match candidate_cancellation(spec.trigger, spec.bypass_duplicate_suppression) {
            CandidateCancellation::AllInflight => cancel_inflight_work(state),
            CandidateCancellation::ExtractionOnly => cancel_pending_extraction(state),
        }
        let attempt = state.pipeline.extract(
            spec.trigger,
            spec.process_id,
            spec.source_root_window,
            spec.pointer,
            spec.selection_rect,
        );
        state.pending = Some(PendingAttempt {
            attempt,
            spec,
            config_generation: state.config_generation,
        });
        let _ = now;
        let _ = hwnd;
    }

    /// Cancel work which may still produce events without changing the
    /// completed result currently presented to the user.
    ///
    /// `popup`, `popup_job_id`, `popup_anchor`, and `last_request` describe
    /// the presented result rather than the candidate extraction. They are
    /// replaced only after a new request passes the request gate.
    fn cancel_inflight_work(state: &mut ShellState) {
        cancel_pending_extraction(state);
        if let Some(token) = state.provider_cancellation.take() {
            token.cancel();
        }
        let _ = state.coordinator.cancel_active();
        state.active_request = None;
        state.active_popup_id = None;
        state.popup_job_id = None;
        state.stream_output.clear();
    }

    fn cancel_pending_extraction(state: &mut ShellState) {
        state.pipeline.cancel_extraction();
        state.pending = None;
    }

    /// Invalidate only hover work. Pointer movement can make an emitted hover
    /// candidate stale while a Selection or Manual request remains
    /// authoritative; similarly, disabling Hover must not interrupt those
    /// higher-priority requests.
    fn cancel_hover_extraction(state: &mut ShellState) {
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.spec.trigger == TriggerKind::Hover)
        {
            cancel_pending_extraction(state);
        }
    }

    fn cancel_hover_work(state: &mut ShellState) {
        cancel_hover_extraction(state);
        let ids: Vec<_> = state
            .popups
            .iter()
            .filter(|entry| entry.presented_trigger == Some(TriggerKind::Hover))
            .map(|entry| entry.id)
            .collect();
        for id in ids {
            remove_popup(state, id, true);
        }
    }

    fn translation_enabled(rest_enabled: bool) -> bool {
        !rest_enabled
    }

    fn set_rest_mode(hwnd: HWND, state: &mut ShellState, enabled: bool) {
        if state.rest_enabled == enabled {
            return;
        }
        state.rest_enabled = enabled;
        cancel_inflight_work(state);
        state.pending_profile_choice = None;
        state.last_request = None;
        state.popup_job_id = None;
        state.last_selection_rect = None;
        let ids: Vec<_> = state.popups.iter().map(|entry| entry.id).collect();
        for id in ids {
            remove_popup(state, id, true);
        }
        state.mouse = mouse::MouseState::new();
        state
            .mouse
            .set_hover_enabled(state.hover_enabled, Instant::now());
        state.hover_trace_deadline = None;
        unsafe {
            let _ = KillTimer(Some(hwnd), mouse::TIMER_SELECTION);
            let _ = KillTimer(Some(hwnd), mouse::TIMER_HOVER);
        }
        state
            .tray
            .update_status(state.hover_enabled, state.rest_enabled);
        runtime_trace::record(if enabled {
            "rest_enabled"
        } else {
            "rest_disabled"
        });
    }

    fn drain_pipeline_events(hwnd: HWND, state: &mut ShellState) {
        while let Ok(event) = state.events.try_recv() {
            match event {
                PipelineEvent::Extraction {
                    attempt,
                    trigger,
                    process_id: _,
                    pointer,
                    selection_rect,
                    result,
                } => handle_extraction(
                    hwnd,
                    state,
                    ExtractionCompleted {
                        attempt,
                        trigger,
                        pointer,
                        selection_rect,
                        result,
                    },
                ),
                PipelineEvent::Delta { job_id, delta } => {
                    if is_current_presentation(&state.coordinator, state.popup_job_id, job_id) {
                        runtime_trace::record_id("delta_received", job_id);
                        state.stream_output.push_str(&delta);
                        if let Some(id) = state.active_popup_id {
                            if let Some(entry) = popup_entry_mut(state, id) {
                                entry.popup.update(&delta);
                            }
                        }
                    }
                }
                PipelineEvent::Finished { job_id, result } => {
                    handle_finished(state, job_id, result);
                }
            }
        }
    }

    fn drain_config_changes(hwnd: HWND, state: &mut ShellState) {
        let mut changes = Vec::new();
        if let Some(watcher) = state.config_watcher.as_ref() {
            while let Ok(change) = watcher.try_recv() {
                changes.push(change);
            }
        }
        for change in changes {
            match change {
                config_reload::ConfigChange::Loaded(config) => {
                    apply_config(hwnd, state, config);
                }
                config_reload::ConfigChange::Invalid => {
                    // Do not replace the parsed config or gate.  Automatic
                    // jobs become silent and Manual reports this local error.
                    let chooser_was_open = state.pending_profile_choice.is_some();
                    let chooser_id = state
                        .pending_profile_choice
                        .as_ref()
                        .map(|choice| choice.popup_id);
                    state.config_generation = state.config_generation.wrapping_add(1).max(1);
                    state.runtime.startup_error = Some("Configuration file is invalid".to_owned());
                    cancel_inflight_work(state);
                    state.pending_profile_choice = None;
                    if chooser_was_open {
                        if let Some(id) = chooser_id {
                            remove_popup(state, id, true);
                        }
                    }
                    state.cache = ResultCache::default();
                }
            }
        }
    }

    fn reload_config_from_disk(hwnd: HWND, state: &mut ShellState) -> bool {
        let Some(path) = default_config_path() else {
            return false;
        };
        let Ok(config) = selection_core::AppConfig::load(path) else {
            return false;
        };
        apply_config(hwnd, state, config);
        true
    }

    fn apply_config(hwnd: HWND, state: &mut ShellState, config: selection_core::AppConfig) {
        // One atomic save can yield more than one directory notification.
        // Reapplying identical configuration would needlessly cancel a slow
        // provider stream and leave its loading surface without a result.
        if resident_runtime_config_eq(&config, &state.runtime.config) {
            // Manager-only presentation settings share config.toml but are
            // deliberately outside the resident runtime contract. Record the
            // new value so repeated notifications are stable without
            // cancelling work, clearing cache, rereading credentials, or
            // rebuilding the provider.
            state.runtime.config.ui = config.ui;
            runtime_trace::record("config_ui_only_applied");
            return;
        }
        state.config_generation = state.config_generation.wrapping_add(1).max(1);
        runtime_trace::record("config_runtime_reload");
        cancel_inflight_work(state);
        state.cache = ResultCache::default();
        state
            .hotkeys
            .reregister(hwnd, &config.hotkeys.cycle_profiles);
        apply_provider_runtime(state, &config);
        state.runtime.config = config;
        if state
            .active_prompt_id
            .as_deref()
            .is_some_and(|id| state.runtime.config.profile(id).is_none())
        {
            state.active_prompt_id = None;
        }
        let replacement_prompt = state.last_request.as_ref().and_then(|request| {
            state
                .runtime
                .config
                .profile(&request.prompt_id)
                .is_none()
                .then(|| selected_prompt_id(state, request.trigger))
        });
        if let (Some(request), Some(prompt_id)) = (state.last_request.as_mut(), replacement_prompt)
        {
            request.prompt_id = prompt_id;
        }
        if state.pending_profile_choice.is_some() {
            let ordered = profile_chooser_order(&state.runtime.config.profiles);
            let profile_ids: Vec<String> = ordered
                .iter()
                .map(|&index| state.runtime.config.profiles[index].id.clone())
                .collect();
            let profile_names: Vec<String> = ordered
                .iter()
                .map(|&index| state.runtime.config.profiles[index].name.clone())
                .collect();
            let chooser_id = state
                .pending_profile_choice
                .as_ref()
                .map(|choice| choice.popup_id);
            let refreshed = chooser_id
                .and_then(|id| popup_entry_mut(state, id))
                .is_some_and(|entry| entry.popup.show_profile_choices(&profile_names));
            if refreshed {
                if let Some(choice) = state.pending_profile_choice.as_mut() {
                    choice.pending.config_generation = state.config_generation;
                    choice.profile_ids = profile_ids;
                }
            } else {
                if let Some(id) = chooser_id {
                    remove_popup(state, id, true);
                }
            }
        }
    }

    fn resident_runtime_config_eq(
        left: &selection_core::AppConfig,
        right: &selection_core::AppConfig,
    ) -> bool {
        left.profiles == right.profiles
            && left.defaults == right.defaults
            && left.provider == right.provider
            && left.hotkeys == right.hotkeys
    }

    fn apply_provider_runtime(state: &mut ShellState, config: &selection_core::AppConfig) {
        let provider_runtime = (state.runtime.provider_reloader)(config);
        state.runtime.request_gate = RequestGate::with_optional_provider(
            provider_runtime.provider_config,
            config.profiles.clone(),
        );
        state
            .pipeline
            .set_provider(provider_runtime.provider.clone());
        state.runtime.provider = provider_runtime.provider;
        state.runtime.startup_error = provider_runtime.error;
    }

    fn refresh_provider(state: &mut ShellState) {
        let chooser_was_open = state.pending_profile_choice.is_some();
        state.config_generation = state.config_generation.wrapping_add(1).max(1);
        cancel_inflight_work(state);
        state.cache = ResultCache::default();
        let config = state.runtime.config.clone();
        apply_provider_runtime(state, &config);
        if chooser_was_open {
            if let Some(id) = state
                .pending_profile_choice
                .take()
                .map(|choice| choice.popup_id)
            {
                remove_popup(state, id, true);
            }
        }
    }

    struct ExtractionCompleted {
        attempt: u64,
        trigger: TriggerKind,
        pointer: Option<ScreenPoint>,
        selection_rect: Option<selection_core::ScreenRect>,
        result: selection_platform_interface::ExtractionResult,
    }

    fn take_matching_pending(
        slot: &mut Option<PendingAttempt>,
        attempt: u64,
        trigger: TriggerKind,
        config_generation: u64,
    ) -> Option<PendingAttempt> {
        let matches = slot.as_ref().is_some_and(|pending| {
            pending.attempt == attempt
                && pending.spec.trigger == trigger
                && pending.config_generation == config_generation
        });
        if !matches {
            return None;
        }
        slot.take()
    }

    fn handle_extraction(hwnd: HWND, state: &mut ShellState, event: ExtractionCompleted) {
        let ExtractionCompleted {
            attempt,
            trigger,
            pointer,
            selection_rect,
            result,
        } = event;
        // Only the event matching the current candidate may consume it. A
        // completion already queued by a cancelled extraction can otherwise
        // arrive first and accidentally discard its replacement.
        let config_generation = state.config_generation;
        let Some(pending) =
            take_matching_pending(&mut state.pending, attempt, trigger, config_generation)
        else {
            return;
        };
        let anchor = pointer
            .map(|point| popup::Point {
                x: point.x,
                y: point.y,
            })
            .or_else(|| cursor_position().ok())
            .unwrap_or(popup::Point { x: 0, y: 0 });
        // A reload can invalidate the provider or credentials after an
        // extraction was queued. Recheck immediately before admission.
        if let Some(error) = state.runtime.startup_error.clone() {
            if trigger == TriggerKind::Manual {
                show_local_error(hwnd, state, attempt, anchor, &error);
            }
            return;
        }
        let text = match result {
            Ok(mut text) => {
                runtime_trace::record("extraction_delivered_success");
                if text.screen_rect.is_none() {
                    text.screen_rect = selection_rect;
                }
                text
            }
            Err(error) => {
                runtime_trace::record("extraction_delivered_failure");
                if trigger == TriggerKind::Manual {
                    show_local_error(hwnd, state, attempt, anchor, extraction_error(error));
                }
                return;
            }
        };
        if trigger == TriggerKind::Selection {
            let preflight = JobInput::new(0, trigger, text.clone(), pending.spec.prompt_id.clone());
            if state
                .runtime
                .request_gate
                .prepare(&preflight, 0, false)
                .is_err()
            {
                runtime_trace::record("selection_chooser_target_rejected");
                return;
            }
            let ordered = profile_chooser_order(&state.runtime.config.profiles);
            let profile_ids: Vec<String> = ordered
                .iter()
                .map(|&index| state.runtime.config.profiles[index].id.clone())
                .collect();
            let profile_names: Vec<String> = ordered
                .iter()
                .map(|&index| state.runtime.config.profiles[index].name.clone())
                .collect();
            let admission = ensure_popup(hwnd, state, anchor, None, None, false);
            let popup_id = admission.map(|admission| admission.popup_id);
            let shown = popup_id
                .and_then(|id| popup_entry_mut(state, id))
                .is_some_and(|entry| entry.popup.show_profile_choices(&profile_names));
            if !shown {
                if let Some(id) = popup_id {
                    remove_popup(state, id, true);
                }
                if !state.popup_failure_reported {
                    state.popup_failure_reported = true;
                    show_diagnostic(ResidentDiagnostic::ResultWindowUnavailable);
                }
                return;
            }
            state.popup_failure_reported = false;
            state.popup_job_id = None;
            state.presented_trigger = Some(TriggerKind::Selection);
            state.popup_guard_root_window = pending.spec.foreground_guard_root_window;
            state.pending_profile_choice = Some(PendingProfileChoice {
                popup_id: popup_id.expect("shown chooser has a popup"),
                pending,
                text,
                anchor,
                profile_ids,
            });
            runtime_trace::record("selection_profile_chooser_shown");
            return;
        }
        admit_extracted_request(hwnd, state, pending, text, anchor);
    }

    fn admit_extracted_request(
        hwnd: HWND,
        state: &mut ShellState,
        pending: PendingAttempt,
        text: TextContext,
        anchor: popup::Point,
    ) {
        let attempt = pending.attempt;
        let trigger = pending.spec.trigger;
        let process_id = pending.spec.process_id;
        let foreground_guard_root_window = pending.spec.foreground_guard_root_window;
        let foreground = unsafe { GetForegroundWindow() };
        let foreground_is_popup = state
            .popups
            .iter()
            .any(|entry| entry.popup.owns_window(foreground));
        if !hover_foreground_guard_is_current(
            trigger,
            foreground_guard_root_window,
            root_window(foreground).0 as isize,
            foreground_is_popup,
        ) {
            runtime_trace::record("hover_admission_foreground_changed");
            return;
        }
        let anchor = match pending.spec.source_popup_id {
            Some(source_id) => {
                let (status, cascade_anchor) = match popup_entry(state, source_id) {
                    Some(source) => {
                        let cascade_anchor = source.popup.cascade_anchor();
                        (
                            cascade_source_status(
                                true,
                                source.popup.is_result_surface_available(),
                                source.popup.is_completed(),
                                cascade_anchor.is_some(),
                            ),
                            cascade_anchor,
                        )
                    }
                    None => (cascade_source_status(false, false, false, false), None),
                };
                match status {
                    CascadeSourceStatus::Ready => {
                        cascade_anchor.expect("ready cascade source has an anchor")
                    }
                    CascadeSourceStatus::Missing => {
                        runtime_trace::record("popup_cascade_source_missing");
                        return;
                    }
                    CascadeSourceStatus::Unavailable => {
                        runtime_trace::record("popup_cascade_source_unavailable");
                        return;
                    }
                }
            }
            None => anchor,
        };
        let prompt_id = pending.spec.prompt_id.clone();
        // Validate all request content before asking Coordinator to supersede
        // the currently presented job. RequestGate is pure; the real job ID
        // is bound to this validated payload after coordination accepts.
        let preflight = JobInput::new(0, trigger, text.clone(), prompt_id.clone());
        let preflight_prepared = match state.runtime.request_gate.prepare(&preflight, 0, false) {
            Ok(prepared) => {
                runtime_trace::record("request_preflight_admitted");
                prepared
            }
            Err(rejection) => {
                runtime_trace::record("request_preflight_rejected");
                if trigger == TriggerKind::Manual {
                    let message = state
                        .runtime
                        .startup_error
                        .clone()
                        .unwrap_or_else(|| request_error(rejection).to_owned());
                    show_local_error(hwnd, state, attempt, anchor, &message);
                }
                return;
            }
        };
        let checked_at = Instant::now();
        let start_check = if pending.spec.bypass_duplicate_suppression {
            state
                .coordinator
                .can_start_explicit(trigger, process_id, &text, checked_at)
        } else {
            state
                .coordinator
                .can_start(trigger, process_id, &text, checked_at)
        };
        if let Err(rejection) = start_check {
            runtime_trace::record("request_admission_rejected");
            if trigger == TriggerKind::Manual {
                show_local_error(hwnd, state, attempt, anchor, &format!("{rejection:?}"));
            }
            return;
        }
        runtime_trace::record("request_admission_accepted");
        let cache_key = CacheKey::from_prepared_with_identity(
            &preflight_prepared,
            format!("{:?}", text.source),
            format!(
                "{}|{}",
                state.runtime.config.provider.endpoint,
                preflight_prepared.model()
            ),
        );
        let cached_output = state.cache.get(&cache_key).map(|output| {
            selection_core::normalize_terminal_response(preflight_prepared.prompt_id(), output)
        });
        let mut surface = match ensure_result_surface(hwnd, state, anchor, &pending.spec) {
            Ok(surface) => surface,
            Err(ResultSurfaceError::ProtectedCapacity) => {
                runtime_trace::record("popup_capacity_protected");
                return;
            }
            Err(ResultSurfaceError::Unavailable) => {
                if !state.popup_failure_reported {
                    state.popup_failure_reported = true;
                    show_diagnostic(ResidentDiagnostic::ResultWindowUnavailable);
                }
                return;
            }
        };
        if !present_staged_popup(state, &mut surface) {
            if !state.popup_failure_reported {
                state.popup_failure_reported = true;
                show_diagnostic(ResidentDiagnostic::ResultWindowUnavailable);
            }
            return;
        }
        let transaction = commit_after_result_surface(Some(surface), || {
            if pending.spec.bypass_duplicate_suppression {
                state.coordinator.start_explicit(
                    trigger,
                    process_id,
                    text.clone(),
                    prompt_id,
                    checked_at,
                )
            } else {
                state
                    .coordinator
                    .start(trigger, process_id, text.clone(), prompt_id, checked_at)
            }
        });
        let Some((mut surface, handle)) = transaction else {
            if !state.popup_failure_reported {
                state.popup_failure_reported = true;
                show_diagnostic(ResidentDiagnostic::ResultWindowUnavailable);
            }
            return;
        };
        // `can_start*` ran with identical inputs on this same message-loop
        // thread, so no state can change between the check and commit.
        let handle = handle.expect("same-thread Coordinator preflight must remain valid");
        let prepared = preflight_prepared.bind_job_id(handle.input.id);
        state.popup_failure_reported = false;

        // The staged surface was already shown reversibly while the previous
        // request remained authoritative. Only now may its registry commit
        // and the previous provider cancellation become visible state.
        commit_presented_staged_popup(state, &mut surface);
        if let Some(token) = state.provider_cancellation.take() {
            token.cancel();
        }
        state.active_request = None;
        state.stream_output.clear();
        state.active_popup_id = Some(surface.popup_id);
        state.popup_job_id = Some(handle.input.id);
        initialize_result_surface(
            state,
            &surface,
            displayed_input(prepared.target(), prepared.context()),
            cached_output.as_deref(),
        );

        // This is the commit point for a candidate replacement: extraction
        // produced a valid target, coordination and RequestGate admitted it,
        // and a native result surface is already visible. Until this point
        // the prior popup and Retry/Prompt request remain authoritative.
        state.last_request = Some(pending.spec);
        state.presented_trigger = Some(trigger);
        state.popup_guard_root_window = foreground_guard_root_window;
        state.popup_anchor = anchor;
        let popup_request = state.last_request.clone();
        let popup_text = text.clone();
        if let Some(entry) = popup_entry_mut(state, surface.popup_id) {
            entry.last_request = popup_request;
            entry.last_text = Some(popup_text);
            entry.presented_trigger = Some(trigger);
            entry.guard_root_window = foreground_guard_root_window;
            entry.anchor = anchor;
        }
        state.active_request = Some(ActiveRequest {
            process_id,
            text,
            prepared: prepared.clone(),
            cache_key: cache_key.clone(),
        });
        if let Some(output) = cached_output {
            state.stream_output = output.clone();
            if let Some(active) = state.active_request.as_ref() {
                enqueue_history(state, active, &output, true);
                let _ = state.coordinator.complete(
                    handle.input.id,
                    active.process_id,
                    &active.text,
                    Instant::now(),
                );
            }
            state.active_request = None;
            return;
        }
        state.provider_cancellation = Some(start_provider_after_result_surface(surface, || {
            runtime_trace::record_id("provider_start", handle.input.id);
            state.pipeline.stream(handle.input.id, prepared)
        }));
    }

    /// Retry and Prompt are operations on the admitted text owned by one
    /// popup. Re-extracting the old screen coordinate is unsafe for cascades:
    /// an outside-click may already have closed the source popup and exposed
    /// unrelated text beneath it.
    fn replay_popup(hwnd: HWND, state: &mut ShellState, popup_id: popup::PopupId, cycle: bool) {
        let Some((mut spec, text, anchor)) = popup_entry(state, popup_id).and_then(|entry| {
            Some((
                entry.last_request.clone()?,
                entry.last_text.clone()?,
                entry.anchor,
            ))
        }) else {
            return;
        };
        if cycle {
            spec.prompt_id = next_prompt(&state.runtime.config, &spec.prompt_id);
            if spec.prompt_id.is_empty() {
                return;
            }
            state.active_prompt_id = Some(spec.prompt_id.clone());
        }
        retarget_popup_replay(&mut spec, popup_id);
        cancel_inflight_work(state);
        admit_extracted_request(
            hwnd,
            state,
            PendingAttempt {
                attempt: 0,
                spec,
                config_generation: state.config_generation,
            },
            text,
            anchor,
        );
    }

    fn retarget_popup_replay(spec: &mut RequestSpec, popup_id: popup::PopupId) {
        spec.bypass_duplicate_suppression = true;
        spec.source_popup_id = None;
        spec.destination_popup_id = Some(popup_id);
    }

    fn begin_selected_profile(
        hwnd: HWND,
        state: &mut ShellState,
        popup_id: popup::PopupId,
        profile_index: usize,
    ) {
        if state
            .pending_profile_choice
            .as_ref()
            .is_none_or(|choice| choice.popup_id != popup_id)
        {
            return;
        }
        let Some(profile_id) = state.pending_profile_choice.as_ref().and_then(|choice| {
            profile_for_generation(
                &choice.profile_ids,
                profile_index,
                choice.pending.config_generation,
                state.config_generation,
            )
        }) else {
            return;
        };
        let Some(mut choice) = state.pending_profile_choice.take() else {
            return;
        };
        if state.runtime.config.profile(&profile_id).is_none() {
            remove_popup(state, popup_id, true);
            return;
        }
        choice.pending.spec.prompt_id = profile_id;
        choice.pending.spec.bypass_duplicate_suppression = true;
        choice.pending.spec.destination_popup_id = Some(popup_id);
        runtime_trace::record("selection_profile_chosen");
        admit_extracted_request(hwnd, state, choice.pending, choice.text, choice.anchor);
    }

    fn profile_for_generation(
        profile_ids: &[String],
        profile_index: usize,
        choice_generation: u64,
        current_generation: u64,
    ) -> Option<String> {
        (choice_generation == current_generation)
            .then(|| profile_ids.get(profile_index).cloned())
            .flatten()
    }

    /// Put the three standard profiles first without changing the configured
    /// order of every other profile. The same ordering is used for labels and
    /// IDs so a click always maps to the intended profile.
    fn profile_chooser_order(profiles: &[selection_core::PromptConfig]) -> Vec<usize> {
        let mut ordered = Vec::with_capacity(profiles.len());
        for prioritized_id in PRIORITIZED_SELECTION_PROFILES {
            ordered.extend(
                profiles
                    .iter()
                    .enumerate()
                    .filter(|(_, profile)| profile.id == prioritized_id)
                    .map(|(index, _)| index),
            );
        }
        ordered.extend(
            profiles
                .iter()
                .enumerate()
                .filter(|(_, profile)| {
                    !PRIORITIZED_SELECTION_PROFILES.contains(&profile.id.as_str())
                })
                .map(|(index, _)| index),
        );
        ordered
    }

    fn handle_finished(
        state: &mut ShellState,
        job_id: u64,
        result: Result<(), selection_platform_interface::ProviderError>,
    ) {
        if !is_current_presentation(&state.coordinator, state.popup_job_id, job_id) {
            return;
        }
        state.provider_cancellation = None;
        match result {
            Ok(()) if !state.stream_output.is_empty() => {
                runtime_trace::record_id("finish_received", job_id);
                let normalized_output = state.active_request.as_ref().map_or_else(
                    || state.stream_output.clone(),
                    |active| {
                        selection_core::normalize_terminal_response(
                            active.prepared.prompt_id(),
                            &state.stream_output,
                        )
                    },
                );
                state.stream_output = normalized_output;
                if let Some(active) = state.active_request.as_ref() {
                    state
                        .cache
                        .insert(active.cache_key.clone(), state.stream_output.clone());
                    enqueue_history(state, active, &state.stream_output, false);
                    let _ = state.coordinator.complete(
                        job_id,
                        active.process_id,
                        &active.text,
                        Instant::now(),
                    );
                }
                state.active_request = None;
                if let Some(id) = state.active_popup_id {
                    let output = state.stream_output.clone();
                    if let Some(entry) = popup_entry_mut(state, id) {
                        entry.popup.set_text(&output);
                    }
                }
            }
            Ok(()) => {
                runtime_trace::record_id("finish_received", job_id);
                let _ = state.coordinator.finish(job_id);
                state.active_request = None;
                if let Some(id) = state.active_popup_id {
                    if let Some(entry) = popup_entry_mut(state, id) {
                        entry.popup.show_local_error("Provider returned no text");
                    }
                }
            }
            Err(error)
                if !matches!(
                    error,
                    selection_platform_interface::ProviderError::Cancelled
                ) =>
            {
                runtime_trace::record_id("finish_received", job_id);
                let _ = state.coordinator.finish(job_id);
                state.active_request = None;
                if let Some(id) = state.active_popup_id {
                    let message = provider_error(error);
                    if let Some(entry) = popup_entry_mut(state, id) {
                        entry.popup.show_local_error(&message);
                    }
                }
            }
            Err(_) => {
                runtime_trace::record_id("finish_received", job_id);
                let _ = state.coordinator.finish(job_id);
                state.active_request = None;
            }
        }
    }

    fn is_current_presentation(
        coordinator: &Coordinator,
        popup_job_id: Option<u64>,
        event_job_id: u64,
    ) -> bool {
        coordinator.is_current(event_job_id) && popup_job_id == Some(event_job_id)
    }

    fn ensure_popup(
        hwnd: HWND,
        state: &mut ShellState,
        anchor: popup::Point,
        destination: Option<popup::PopupId>,
        parent: Option<popup::PopupId>,
        force_new: bool,
    ) -> Option<PopupAdmission> {
        runtime_trace::record("result_surface_ensure_begin");
        if let Some(id) = destination {
            let alive =
                popup_entry_mut(state, id).is_some_and(|entry| entry.popup.reanchor(anchor));
            if alive {
                return Some(PopupAdmission {
                    popup_id: id,
                    created_new: false,
                    staged_entry: None,
                    evict_after_commit: None,
                });
            }
            remove_popup(state, id, false);
        }

        if !force_new {
            if let Some(id) = state
                .popups
                .iter()
                .rev()
                .find(|entry| popup_can_be_reused(entry.popup.is_pinned()))
                .map(|entry| entry.id)
            {
                let alive =
                    popup_entry_mut(state, id).is_some_and(|entry| entry.popup.reanchor(anchor));
                if alive {
                    return Some(PopupAdmission {
                        popup_id: id,
                        created_new: false,
                        staged_entry: None,
                        evict_after_commit: None,
                    });
                }
                remove_popup(state, id, false);
            }
        }

        let active_destination = state.active_popup_id;
        let eviction_candidate = if state.popups.len() >= MAX_VISIBLE_POPUPS {
            let candidates: Vec<_> = state
                .popups
                .iter()
                .map(|entry| PopupEvictionCandidate {
                    id: entry.id,
                    created_order: entry.created_order,
                    pinned: entry.popup.is_pinned(),
                    completed: entry.popup.is_completed(),
                })
                .collect();
            oldest_evictable_popup(&candidates, parent, active_destination)
        } else {
            None
        };
        if state.popups.len() >= MAX_VISIBLE_POPUPS {
            let evicted = eviction_candidate?;
            let id = allocate_popup_id(state);
            let popup = popup::Popup::stage(hwnd, id, anchor).ok()?;
            let staged_entry = PopupEntry {
                id,
                popup,
                last_request: None,
                last_text: None,
                presented_trigger: None,
                guard_root_window: 0,
                anchor,
                parent,
                created_order: id as u64,
            };
            runtime_trace::record("popup_staged_at_capacity");
            return Some(PopupAdmission {
                popup_id: id,
                created_new: true,
                staged_entry: Some(staged_entry),
                evict_after_commit: Some(evicted),
            });
        }

        let id = allocate_popup_id(state);
        let popup = match popup::Popup::show(hwnd, id, anchor) {
            Ok(popup) => popup,
            Err(_) => {
                runtime_trace::record("popup_show_failure");
                return None;
            }
        };
        state.popups.push(PopupEntry {
            id,
            popup,
            last_request: None,
            last_text: None,
            presented_trigger: None,
            guard_root_window: 0,
            anchor,
            parent,
            created_order: id as u64,
        });
        runtime_trace::record("app_stores_popup");
        Some(PopupAdmission {
            popup_id: id,
            created_new: true,
            staged_entry: None,
            evict_after_commit: None,
        })
    }

    fn ensure_result_surface(
        hwnd: HWND,
        state: &mut ShellState,
        anchor: popup::Point,
        spec: &RequestSpec,
    ) -> Result<ResultSurfaceReady, ResultSurfaceError> {
        let force_new = spec.source_popup_id.is_some() && spec.destination_popup_id.is_none();
        if force_new && state.popups.len() >= MAX_VISIBLE_POPUPS {
            let candidates: Vec<_> = state
                .popups
                .iter()
                .map(|entry| PopupEvictionCandidate {
                    id: entry.id,
                    created_order: entry.created_order,
                    pinned: entry.popup.is_pinned(),
                    completed: entry.popup.is_completed(),
                })
                .collect();
            let eviction =
                oldest_evictable_popup(&candidates, spec.source_popup_id, state.active_popup_id);
            if let Some(error) = protected_capacity_error(force_new, state.popups.len(), eviction) {
                return Err(error);
            }
        }
        let admission = ensure_popup(
            hwnd,
            state,
            anchor,
            spec.destination_popup_id,
            spec.source_popup_id,
            force_new,
        )
        .ok_or(ResultSurfaceError::Unavailable)?;
        let available = admission.staged_entry.as_ref().map_or_else(
            || {
                popup_entry(state, admission.popup_id)
                    .is_some_and(|entry| entry.popup.is_result_surface_available())
            },
            |entry| entry.popup.is_result_surface_available(),
        );
        if available {
            Ok(ResultSurfaceReady {
                popup_id: admission.popup_id,
                staged_entry: admission.staged_entry,
                evict_after_commit: admission.evict_after_commit,
            })
        } else {
            if admission.created_new {
                remove_popup(state, admission.popup_id, true);
            }
            Err(ResultSurfaceError::Unavailable)
        }
    }

    fn present_staged_popup(state: &mut ShellState, surface: &mut ResultSurfaceReady) -> bool {
        let Some(staged) = surface.staged_entry.as_mut() else {
            return true;
        };
        let Some(evicted_id) = surface.evict_after_commit else {
            return false;
        };
        let Some(evicted) = popup_entry_mut(state, evicted_id) else {
            return false;
        };
        if !evicted.popup.hide_temporarily() {
            return false;
        }
        if !staged.popup.present_staged() {
            let _ = evicted.popup.present_staged();
            return false;
        }
        true
    }

    fn rollback_staged_popup(state: &mut ShellState, surface: &mut ResultSurfaceReady) {
        if let Some(staged) = surface.staged_entry.as_mut() {
            let _ = staged.popup.hide_temporarily();
        }
        if let Some(evicted_id) = surface.evict_after_commit {
            if let Some(evicted) = popup_entry_mut(state, evicted_id) {
                let _ = evicted.popup.present_staged();
            }
        }
    }

    fn commit_presented_staged_popup(state: &mut ShellState, surface: &mut ResultSurfaceReady) {
        let Some(staged) = surface.staged_entry.take() else {
            return;
        };
        let evicted_id = surface
            .evict_after_commit
            .expect("staged capacity popup has an eviction target");
        assert!(
            popup_entry(state, evicted_id).is_some(),
            "same-thread staged eviction target remains registered"
        );
        remove_popup(state, evicted_id, true);
        state.popups.push(staged);
        runtime_trace::record("popup_staged_commit");
    }

    fn initialize_result_surface(
        state: &mut ShellState,
        surface: &ResultSurfaceReady,
        input: &str,
        cached_output: Option<&str>,
    ) {
        let popup = popup_entry_mut(state, surface.popup_id)
            .map(|entry| &mut entry.popup)
            .expect("result-surface capability requires a live popup");
        popup.set_input(input);
        if let Some(output) = cached_output {
            popup.set_text(output);
        } else {
            popup.show_loading();
        }
    }

    fn displayed_input<'a>(target: &'a str, context: Option<&'a str>) -> &'a str {
        context.unwrap_or(target)
    }

    fn enqueue_history(
        state: &ShellState,
        active: &ActiveRequest,
        output: &str,
        served_from_cache: bool,
    ) {
        if selection_core::normalize::normalize_target(output).is_empty() {
            return;
        }
        let Some(history) = state.runtime.history.as_ref() else {
            return;
        };
        let entry = CompletedEntry {
            created_at_utc: canonical_utc_now(),
            source: active.text.source,
            target: active.prepared.target().to_owned(),
            context: active.prepared.context().map(str::to_owned),
            output: output.to_owned(),
            prompt_id: active.prepared.prompt_id().to_owned(),
            model: active.prepared.model().to_owned(),
            served_from_cache,
        };
        if history.insert_completed(entry).is_err() {
            // Keep the UI silent and avoid logging target/output/provider
            // details. Persistence is best-effort and never blocks usage.
            eprintln!("Selection Translate history queue unavailable");
        }
    }

    fn show_local_error(
        hwnd: HWND,
        state: &mut ShellState,
        job_id: u64,
        anchor: popup::Point,
        message: &str,
    ) {
        if let Some(admission) = ensure_popup(hwnd, state, anchor, None, None, false) {
            let mut surface = ResultSurfaceReady {
                popup_id: admission.popup_id,
                staged_entry: admission.staged_entry,
                evict_after_commit: admission.evict_after_commit,
            };
            if !present_staged_popup(state, &mut surface) {
                rollback_staged_popup(state, &mut surface);
                if !state.popup_failure_reported {
                    state.popup_failure_reported = true;
                    show_diagnostic(ResidentDiagnostic::ResultWindowUnavailable);
                }
                return;
            }
            commit_presented_staged_popup(state, &mut surface);
            let id = surface.popup_id;
            state.popup_job_id = Some(job_id);
            state.presented_trigger = None;
            state.popup_guard_root_window = 0;
            if let Some(entry) = popup_entry_mut(state, id) {
                entry.popup.show_local_error(message);
                entry.presented_trigger = None;
                entry.guard_root_window = 0;
            }
            state.active_popup_id = Some(id);
            state.popup_failure_reported = false;
        } else if !state.popup_failure_reported {
            state.popup_failure_reported = true;
            show_diagnostic(ResidentDiagnostic::ResultWindowUnavailable);
        }
    }

    fn next_prompt(config: &selection_core::AppConfig, current: &str) -> String {
        if config.profiles.is_empty() {
            return String::new();
        }
        let Some(index) = config
            .profiles
            .iter()
            .position(|profile| profile.id == current)
        else {
            return config
                .profiles
                .first()
                .map_or_else(String::new, |profile| profile.id.clone());
        };
        config.profiles[(index + 1) % config.profiles.len()]
            .id
            .clone()
    }

    fn selected_prompt_id(state: &ShellState, trigger: TriggerKind) -> String {
        state
            .active_prompt_id
            .as_deref()
            .filter(|id| state.runtime.config.profile(id).is_some())
            .map_or_else(
                || state.runtime.config.default_profile_id(trigger).to_owned(),
                str::to_owned,
            )
    }

    fn cycle_prompt(state: &mut ShellState) {
        let current = state
            .active_prompt_id
            .as_deref()
            .or_else(|| {
                state
                    .last_request
                    .as_ref()
                    .map(|request| request.prompt_id.as_str())
            })
            .unwrap_or_else(|| {
                state
                    .runtime
                    .config
                    .default_profile_id(TriggerKind::Selection)
            });
        let next = next_prompt(&state.runtime.config, current);
        if next.is_empty() {
            return;
        }
        state.active_prompt_id = Some(next.clone());
        if let Some(request) = state.last_request.as_mut() {
            request.prompt_id = next;
        }
    }

    fn request_error(error: RequestRejection) -> &'static str {
        match error {
            RequestRejection::MissingTarget => "No selectable text was detected",
            RequestRejection::TargetTooLong { .. } => "Selected text is too long",
            RequestRejection::Cancelled => "Request cancelled",
            RequestRejection::StaleJob { .. } => "Request became stale",
            RequestRejection::MissingPrompt | RequestRejection::InvalidPrompt => {
                "Prompt configuration is invalid"
            }
            RequestRejection::MissingProviderConfig | RequestRejection::InvalidProviderConfig => {
                "Provider configuration is unavailable"
            }
        }
    }

    fn extraction_error(error: selection_platform_interface::ExtractionFailure) -> &'static str {
        match error {
            selection_platform_interface::ExtractionFailure::UnsupportedPattern => {
                "This control does not expose selectable text"
            }
            selection_platform_interface::ExtractionFailure::EmptyRange => {
                "No selectable text was detected"
            }
            selection_platform_interface::ExtractionFailure::PermissionDenied => {
                "Text access was denied"
            }
            selection_platform_interface::ExtractionFailure::StaleElement => {
                "The text target changed before it could be read"
            }
            selection_platform_interface::ExtractionFailure::Platform => {
                "Text extraction is unavailable"
            }
        }
    }

    fn provider_error(error: selection_platform_interface::ProviderError) -> String {
        match error {
            selection_platform_interface::ProviderError::Cancelled => "Request cancelled".into(),
            selection_platform_interface::ProviderError::Configuration => {
                "Provider configuration is invalid".into()
            }
            selection_platform_interface::ProviderError::Authentication => {
                "Provider authentication failed".into()
            }
            selection_platform_interface::ProviderError::HttpStatus(status) => {
                format!("Provider returned HTTP status {status}")
            }
            selection_platform_interface::ProviderError::RateLimited => {
                "Provider rate limit exceeded".into()
            }
            selection_platform_interface::ProviderError::Dns => {
                "Provider host could not be resolved".into()
            }
            selection_platform_interface::ProviderError::Tls => {
                "Secure provider connection failed".into()
            }
            selection_platform_interface::ProviderError::Timeout => {
                "Provider request timed out".into()
            }
            selection_platform_interface::ProviderError::Transport => {
                "Provider connection failed".into()
            }
            selection_platform_interface::ProviderError::MalformedResponse => {
                "Provider returned malformed data".into()
            }
            selection_platform_interface::ProviderError::IncompleteResponse => {
                "Provider response ended before completion".into()
            }
            selection_platform_interface::ProviderError::ResponseTooLarge => {
                "Provider response exceeded the safety limit".into()
            }
            selection_platform_interface::ProviderError::Unavailable => {
                "Provider is unavailable".into()
            }
            selection_platform_interface::ProviderError::InvalidResponse => {
                "Provider returned invalid data".into()
            }
            selection_platform_interface::ProviderError::Local(_) => {
                "Provider request failed".into()
            }
        }
    }

    fn open_manager() -> std::io::Result<()> {
        let executable = std::env::current_exe()?;
        let manager = executable
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("selection-translate-manager.exe");
        Command::new(manager).spawn().map(|_| ())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn pending(attempt: u64, trigger: TriggerKind, generation: u64) -> PendingAttempt {
            PendingAttempt {
                attempt,
                spec: RequestSpec {
                    trigger,
                    process_id: 42,
                    source_root_window: 42,
                    foreground_guard_root_window: 42,
                    pointer: Some(ScreenPoint::new(10, 20)),
                    selection_rect: None,
                    prompt_id: "translate".to_owned(),
                    bypass_duplicate_suppression: false,
                    source_popup_id: None,
                    destination_popup_id: None,
                },
                config_generation: generation,
            }
        }

        #[test]
        fn remembered_selection_rect_is_only_reused_in_same_root_window() {
            let rect = selection_core::ScreenRect::new(1, 2, 3, 4);
            assert_eq!(selection_rect_for_root(Some((rect, 10)), 10), Some(rect));
            assert_eq!(selection_rect_for_root(Some((rect, 10)), 11), None);
            assert_eq!(selection_rect_for_root(Some((rect, 0)), 0), None);
        }

        #[test]
        fn manager_language_only_change_is_not_a_resident_runtime_change() {
            let english = selection_core::AppConfig::default();
            let mut chinese = english.clone();
            chinese.ui.manager_language = selection_core::UiLanguage::SimplifiedChinese;
            assert!(resident_runtime_config_eq(&english, &chinese));

            chinese.provider.model.push_str("-changed");
            assert!(!resident_runtime_config_eq(&english, &chinese));
        }

        #[test]
        fn stale_extraction_does_not_consume_current_candidate() {
            let mut slot = Some(pending(2, TriggerKind::Selection, 7));

            assert!(take_matching_pending(&mut slot, 1, TriggerKind::Selection, 7).is_none());
            assert!(slot.is_some());
            assert!(take_matching_pending(&mut slot, 2, TriggerKind::Hover, 7).is_none());
            assert!(slot.is_some());
            assert!(take_matching_pending(&mut slot, 2, TriggerKind::Selection, 6).is_none());
            assert!(slot.is_some());
        }

        #[test]
        fn matching_extraction_consumes_current_candidate_once() {
            let mut slot = Some(pending(2, TriggerKind::Manual, 7));

            let accepted = take_matching_pending(&mut slot, 2, TriggerKind::Manual, 7)
                .expect("matching extraction should be accepted");
            assert_eq!(accepted.attempt, 2);
            assert!(slot.is_none());
            assert!(take_matching_pending(&mut slot, 2, TriggerKind::Manual, 7).is_none());
        }

        #[test]
        fn chooser_maps_visible_index_only_for_current_config_generation() {
            let profiles = vec!["translate".to_owned(), "explain".to_owned()];

            assert_eq!(
                profile_for_generation(&profiles, 1, 7, 7),
                Some("explain".to_owned())
            );
            assert_eq!(profile_for_generation(&profiles, 2, 7, 7), None);
            assert_eq!(profile_for_generation(&profiles, 0, 6, 7), None);
        }

        #[test]
        fn chooser_prioritizes_standard_profiles_and_preserves_other_order() {
            let profiles = vec![
                selection_core::PromptConfig::new("custom-first"),
                selection_core::PromptConfig::new("code-specialist"),
                selection_core::PromptConfig::new("custom-second"),
                selection_core::PromptConfig::new("linguist-analysis"),
                selection_core::PromptConfig::new("concise-explanation"),
            ];

            let ordered = profile_chooser_order(&profiles);
            let ids: Vec<&str> = ordered
                .iter()
                .map(|&index| profiles[index].id.as_str())
                .collect();
            assert_eq!(
                ids,
                vec![
                    "linguist-analysis",
                    "code-specialist",
                    "concise-explanation",
                    "custom-first",
                    "custom-second",
                ]
            );
        }

        #[test]
        fn chooser_omits_missing_standard_profiles_without_reordering_custom_profiles() {
            let profiles = vec![
                selection_core::PromptConfig::new("custom-first"),
                selection_core::PromptConfig::new("concise-explanation"),
                selection_core::PromptConfig::new("custom-second"),
            ];

            let ordered = profile_chooser_order(&profiles);
            let ids: Vec<&str> = ordered
                .iter()
                .map(|&index| profiles[index].id.as_str())
                .collect();
            assert_eq!(
                ids,
                vec!["concise-explanation", "custom-first", "custom-second"]
            );
        }

        #[test]
        fn rest_mode_blocks_translation_inputs_only_when_enabled() {
            assert!(translation_enabled(false));
            assert!(!translation_enabled(true));
        }

        #[test]
        fn pinned_popups_are_neither_reused_nor_evicted() {
            assert!(popup_can_be_reused(false));
            assert!(!popup_can_be_reused(true));
            assert!(popup_can_be_evicted(2, Some(1), Some(3), false, true));
            assert!(!popup_can_be_evicted(1, Some(1), Some(3), false, true));
            assert!(!popup_can_be_evicted(3, Some(1), Some(3), false, true));
            assert!(!popup_can_be_evicted(2, Some(1), Some(3), true, true));
            assert!(!popup_can_be_evicted(2, Some(1), Some(3), false, false));
        }

        #[test]
        fn cascade_source_is_revalidated_after_extraction_before_admission() {
            assert_eq!(
                cascade_source_status(false, false, false, false),
                CascadeSourceStatus::Missing
            );
            assert_eq!(
                cascade_source_status(true, false, true, true),
                CascadeSourceStatus::Unavailable,
                "a dead source popup must be rejected before provider admission"
            );
            assert_eq!(
                cascade_source_status(true, true, false, true),
                CascadeSourceStatus::Unavailable,
                "a loading/streaming source popup must not start a cascade"
            );
            assert_eq!(
                cascade_source_status(true, true, true, false),
                CascadeSourceStatus::Unavailable,
                "a source without a usable native anchor must not start a cascade"
            );
            assert_eq!(
                cascade_source_status(true, true, true, true),
                CascadeSourceStatus::Ready
            );
        }

        #[test]
        fn full_popup_registry_selects_only_oldest_eligible_eviction() {
            let candidates: Vec<_> = (0..MAX_VISIBLE_POPUPS)
                .map(|index| PopupEvictionCandidate {
                    id: index + 1,
                    created_order: index as u64,
                    pinned: index == 0,
                    completed: index != 1,
                })
                .collect();

            assert_eq!(candidates.len(), MAX_VISIBLE_POPUPS);
            assert_eq!(
                oldest_evictable_popup(&candidates, Some(3), Some(99)),
                Some(4),
                "pinned, incomplete, parent, and active entries are protected"
            );
            assert_eq!(
                oldest_evictable_popup(&candidates, Some(3), Some(4)),
                None,
                "the active destination must not be evicted"
            );
        }

        #[test]
        fn full_popup_registry_has_no_admission_when_every_entry_is_protected() {
            let candidates: Vec<_> = (0..MAX_VISIBLE_POPUPS)
                .map(|index| PopupEvictionCandidate {
                    id: index + 1,
                    created_order: index as u64,
                    pinned: true,
                    completed: true,
                })
                .collect();

            assert_eq!(candidates.len(), MAX_VISIBLE_POPUPS);
            assert_eq!(oldest_evictable_popup(&candidates, None, None), None);
            assert_eq!(
                protected_capacity_error(true, candidates.len(), None),
                Some(ResultSurfaceError::ProtectedCapacity),
                "a protected capacity rejection must stay local and never become a native-window diagnostic"
            );
            assert_eq!(
                candidates.len(),
                MAX_VISIBLE_POPUPS,
                "a full all-pinned registry must not grow past the capacity limit"
            );
        }

        #[test]
        fn staged_popup_uses_a_fresh_monotonic_id_without_growing_registry() {
            let mut ids: Vec<_> = (1..=MAX_VISIBLE_POPUPS).collect();
            let before = ids.len();
            let evicted = ids[1];
            let (staged, next_id) = next_unused_popup_id(5, &ids);

            ids[1] = staged;
            assert_eq!(ids.len(), before, "staging replaces one entry at commit");
            assert!(staged > MAX_VISIBLE_POPUPS);
            assert!(!ids[..1].contains(&staged));
            assert!(!ids[2..].contains(&staged));
            assert_ne!(staged, evicted);
            assert!(next_id > staged);
        }

        #[test]
        fn popup_replay_targets_owner_without_reextracting_a_parent() {
            let mut request = pending(7, TriggerKind::Hover, 1).spec;
            request.source_popup_id = Some(10);
            retarget_popup_replay(&mut request, 11);
            assert_eq!(request.source_popup_id, None);
            assert_eq!(request.destination_popup_id, Some(11));
            assert!(request.bypass_duplicate_suppression);
        }

        #[test]
        fn popup_input_prefers_full_sentence_and_falls_back_to_target() {
            assert_eq!(
                displayed_input("bank", Some("He sat on the bank by the river.")),
                "He sat on the bank by the river."
            );
            assert_eq!(displayed_input("bank", None), "bank");
        }

        #[test]
        fn manager_start_decision_uses_ready_and_mutex_evidence() {
            assert_eq!(
                plan_resident_start(true, true),
                ResidentStartPlan::UseReadyResident
            );
            assert_eq!(
                plan_resident_start(false, true),
                ResidentStartPlan::WaitForStartingResident
            );
            assert_eq!(
                plan_resident_start(false, false),
                ResidentStartPlan::LaunchSibling
            );
        }

        #[test]
        fn refresh_acknowledgement_never_conflates_missing_or_silent_resident() {
            assert_eq!(
                decode_refresh_response(Ok(ACK_REFRESHED)),
                RefreshOutcome::Acknowledged
            );
            assert_eq!(
                decode_refresh_response(Ok(ACK_REJECTED)),
                RefreshOutcome::Rejected
            );
            assert_eq!(
                decode_refresh_response(Ok(0)),
                RefreshOutcome::Unacknowledged
            );
            assert_eq!(
                decode_refresh_response(Err(RefreshOutcome::ResidentAbsent)),
                RefreshOutcome::ResidentAbsent
            );
        }

        #[test]
        fn popup_admission_failure_preserves_prior_job_and_provider() {
            let at = Instant::now();
            let mut coordinator = Coordinator::new();
            let current_text = TextContext {
                target: "current".to_owned(),
                context: None,
                source: selection_core::ExtractionSource::UiaSelection,
                screen_rect: None,
            };
            let current = coordinator
                .start(TriggerKind::Selection, 42, current_text, "translate", at)
                .expect("current selection starts");
            let replacement_text = TextContext {
                target: "replacement".to_owned(),
                context: Some("whole sentence".to_owned()),
                source: selection_core::ExtractionSource::UiaSelection,
                screen_rect: None,
            };
            coordinator
                .can_start(TriggerKind::Selection, 42, &replacement_text, at)
                .expect("replacement passes non-mutating coordinator preflight");
            let current_provider = CancellationToken::new();
            let mut provider_calls = 0;
            let committed = commit_after_result_surface(None, || {
                provider_calls += 1;
                current_provider.cancel();
                coordinator.start(
                    TriggerKind::Selection,
                    42,
                    replacement_text,
                    "translate",
                    at,
                )
            });

            assert!(committed.is_none());
            assert_eq!(provider_calls, 0);
            assert_eq!(coordinator.active_job_id(), Some(current.input.id));
            assert!(!current.cancellation.is_cancelled());
            assert!(!current_provider.is_cancelled());
        }

        #[test]
        fn delayed_selection_job_remains_the_current_presentation() {
            let started_at = Instant::now();
            let mut coordinator = Coordinator::new();
            let text = TextContext {
                target: "target".to_owned(),
                context: Some("whole sentence".to_owned()),
                source: selection_core::ExtractionSource::UiaSelection,
                screen_rect: None,
            };
            let job = coordinator
                .start(TriggerKind::Selection, 42, text, "translate", started_at)
                .expect("selection job starts");

            let _provider_finishes_later = started_at + Duration::from_secs(30);
            assert!(is_current_presentation(
                &coordinator,
                Some(job.input.id),
                job.input.id
            ));
            assert_eq!(
                candidate_cancellation(TriggerKind::Selection, false),
                CandidateCancellation::ExtractionOnly
            );
            assert_eq!(coordinator.active_job_id(), Some(job.input.id));
        }

        #[test]
        fn explicit_requests_still_cancel_the_previous_job_immediately() {
            assert_eq!(
                candidate_cancellation(TriggerKind::Manual, false),
                CandidateCancellation::AllInflight
            );
            assert_eq!(
                candidate_cancellation(TriggerKind::Selection, true),
                CandidateCancellation::AllInflight
            );
        }

        #[test]
        fn outside_click_retains_only_the_clicked_or_pinned_popups() {
            assert!(is_pointer_button_down(WM_LBUTTONDOWN));
            assert!(is_pointer_button_down(WM_RBUTTONDOWN));
            assert!(is_pointer_button_down(WM_MBUTTONDOWN));
            assert!(is_pointer_button_down(WM_XBUTTONDOWN));
            assert!(!is_pointer_button_down(
                windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE
            ));
            assert!(should_dismiss_for_outside_click(None, 1, false));
            assert!(!should_dismiss_for_outside_click(None, 1, true));
            assert!(!should_dismiss_for_outside_click(Some(2), 2, false));
            assert!(should_dismiss_for_outside_click(Some(2), 1, false));
            assert!(!should_dismiss_for_outside_click(Some(2), 1, true));
        }

        #[test]
        fn hover_popup_foreground_guard_accepts_internal_or_same_root_only() {
            assert!(hover_foreground_guard_is_current(
                TriggerKind::Hover,
                41,
                41,
                false,
            ));
            assert!(!hover_foreground_guard_is_current(
                TriggerKind::Hover,
                41,
                99,
                false,
            ));
            assert!(hover_foreground_guard_is_current(
                TriggerKind::Hover,
                41,
                99,
                true,
            ));
            assert!(!should_dismiss_hover_for_foreground(
                Some(TriggerKind::Hover),
                10,
                10,
                true,
                false,
                false,
            ));
            assert!(should_dismiss_hover_for_foreground(
                Some(TriggerKind::Hover),
                10,
                20,
                true,
                false,
                false,
            ));
            assert!(!should_dismiss_hover_for_foreground(
                Some(TriggerKind::Hover),
                10,
                20,
                true,
                false,
                true,
            ));
            assert!(!should_dismiss_hover_for_foreground(
                Some(TriggerKind::Hover),
                10,
                20,
                true,
                true,
                false,
            ));
        }

        #[test]
        fn visible_diagnostics_are_fixed_and_privacy_safe() {
            let forbidden = ["endpoint", "model", "prompt", "selected text", "api key"];
            for diagnostic in [
                ResidentDiagnostic::FatalStartup,
                ResidentDiagnostic::ManualHotkeyUnavailable,
                ResidentDiagnostic::ResultWindowUnavailable,
            ] {
                let message = diagnostic.message().to_ascii_lowercase();
                for term in forbidden {
                    assert!(!message.contains(term));
                }
            }
        }
    }
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub fn run(_runtime: super::composition::AppRuntime) -> Result<(), &'static str> {
    Err("the resident shell requires Windows")
}
