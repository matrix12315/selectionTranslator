//! Event-driven foreground-window observation for Hover popup lifetime.

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WM_APP,
    };

    pub const WM_FOREGROUND_CHANGED: u32 = WM_APP + 21;
    static EVENT_TARGET: AtomicIsize = AtomicIsize::new(0);

    pub struct ForegroundHook {
        hook: HWINEVENTHOOK,
    }

    impl ForegroundHook {
        pub fn install(target: HWND) -> windows::core::Result<Self> {
            EVENT_TARGET.store(target.0 as isize, Ordering::Release);
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(foreground_event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.0.is_null() {
                EVENT_TARGET.store(0, Ordering::Release);
                return Err(windows::core::Error::from_win32());
            }
            Ok(Self { hook })
        }
    }

    impl Drop for ForegroundHook {
        fn drop(&mut self) {
            EVENT_TARGET.store(0, Ordering::Release);
            unsafe {
                let _ = UnhookWinEvent(self.hook);
            }
        }
    }

    unsafe extern "system" fn foreground_event(
        _hook: HWINEVENTHOOK,
        event: u32,
        foreground: HWND,
        _object_id: i32,
        _child_id: i32,
        _event_thread: u32,
        _event_time: u32,
    ) {
        if event != EVENT_SYSTEM_FOREGROUND || foreground.0.is_null() {
            return;
        }
        let target = EVENT_TARGET.load(Ordering::Acquire);
        if target == 0 {
            return;
        }
        let _ = PostMessageW(
            Some(HWND(target as *mut _)),
            WM_FOREGROUND_CHANGED,
            WPARAM(foreground.0 as usize),
            LPARAM(0),
        );
    }
}

#[cfg(windows)]
pub use windows_impl::*;
