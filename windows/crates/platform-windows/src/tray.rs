//! Notification-area icon and its small resident command menu.

#[cfg(windows)]
mod windows_impl {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, SetForegroundWindow,
        TrackPopupMenu, IDI_APPLICATION, MF_STRING, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_APP,
        WM_COMMAND, WM_LBUTTONDBLCLK, WM_RBUTTONUP,
    };

    pub const TRAY_CALLBACK: u32 = WM_APP + 7;
    pub const OPEN_MANAGER_COMMAND: usize = 0x7201;
    pub const TOGGLE_HOVER_COMMAND: usize = 0x7202;
    pub const TOGGLE_REST_COMMAND: usize = 0x7203;
    pub const EXIT_COMMAND: usize = 0x7204;

    #[derive(Debug)]
    pub struct TrayIcon {
        hwnd: HWND,
        id: u32,
    }

    impl TrayIcon {
        pub fn add(hwnd: HWND) -> windows::core::Result<Self> {
            let icon = Self { hwnd, id: 1 };
            let mut data = icon.data(NIF_MESSAGE | NIF_TIP | NIF_ICON);
            data.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION)? };
            if !unsafe { Shell_NotifyIconW(NIM_ADD, &data).as_bool() } {
                return Err(windows::core::Error::from_win32());
            }
            Ok(icon)
        }

        fn data(&self, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
            let mut data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: self.id,
                uFlags: flags,
                uCallbackMessage: TRAY_CALLBACK,
                ..Default::default()
            };
            let tip: Vec<u16> = "Selection Translate\0".encode_utf16().collect();
            let len = tip.len().min(data.szTip.len());
            data.szTip[..len].copy_from_slice(&tip[..len]);
            data
        }

        pub fn update_status(&self, hover_enabled: bool, rest_enabled: bool) {
            let mut data = self.data(NIF_TIP);
            let text = if rest_enabled {
                "Selection Translate (Rest mode)\0"
            } else if hover_enabled {
                "Selection Translate (Hover on)\0"
            } else {
                "Selection Translate\0"
            };
            let tip: Vec<u16> = text.encode_utf16().collect();
            let len = tip.len().min(data.szTip.len());
            data.szTip[..len].copy_from_slice(&tip[..len]);
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        }

        pub fn remove(&self) {
            let data = self.data(NOTIFY_ICON_DATA_FLAGS(0));
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        }
    }

    impl Drop for TrayIcon {
        fn drop(&mut self) {
            self.remove();
        }
    }

    pub fn handle_callback(hwnd: HWND, lparam: LPARAM, rest_enabled: bool) {
        match lparam.0 as u32 {
            WM_LBUTTONDBLCLK => unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(hwnd),
                    WM_COMMAND,
                    WPARAM(OPEN_MANAGER_COMMAND),
                    LPARAM(0),
                )
                .ok();
            },
            WM_RBUTTONUP => show_menu(hwnd, rest_enabled),
            _ => {}
        }
    }

    fn show_menu(hwnd: HWND, rest_enabled: bool) {
        let menu = match unsafe { CreatePopupMenu() } {
            Ok(menu) => menu,
            Err(error) => {
                eprintln!("could not create tray menu: {error}");
                return;
            }
        };
        let result = unsafe {
            AppendMenuW(menu, MF_STRING, OPEN_MANAGER_COMMAND, w!("Open Manager"))
                .and_then(|_| {
                    AppendMenuW(menu, MF_STRING, TOGGLE_HOVER_COMMAND, w!("Toggle Hover"))
                })
                .and_then(|_| {
                    AppendMenuW(
                        menu,
                        MF_STRING,
                        TOGGLE_REST_COMMAND,
                        if rest_enabled {
                            w!("Disable Rest Mode")
                        } else {
                            w!("Enable Rest Mode")
                        },
                    )
                })
                .and_then(|_| AppendMenuW(menu, MF_STRING, EXIT_COMMAND, w!("Exit")))
        };
        if result.is_ok() {
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point).is_ok() } {
                unsafe {
                    let _ = SetForegroundWindow(hwnd);
                    let _ = TrackPopupMenu(
                        menu,
                        TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                        point.x,
                        point.y,
                        Some(0),
                        hwnd,
                        None,
                    );
                }
            }
        }
        unsafe {
            let _ = DestroyMenu(menu);
        }
    }
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub mod windows_impl {
    #[derive(Debug)]
    pub struct TrayIcon;
}

#[cfg(not(windows))]
pub use windows_impl::*;
