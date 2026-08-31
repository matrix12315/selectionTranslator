//! Native configuration-file watching for the resident process.
//!
//! The watcher observes the directory containing `config.toml` with
//! `ReadDirectoryChangesW`.  It never publishes TOML text or a partially
//! written configuration: the file is parsed and validated before an event
//! is sent to the resident message loop.  An invalid edit therefore leaves
//! the caller's last-known-good configuration untouched.

#![cfg(windows)]

use selection_core::AppConfig;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{GetOverlappedResultEx, OVERLAPPED};

/// Wake-up message consumed by the resident window procedure.
pub const CONFIG_RELOAD_MESSAGE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 32;

/// A validated replacement or a rejected edit.  The invalid variant carries
/// no parser text because diagnostics can accidentally echo sensitive TOML.
#[derive(Debug)]
pub enum ConfigChange {
    Loaded(AppConfig),
    Invalid,
}

/// Owns the watcher thread and its stop signal.
pub struct ConfigWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    receiver: Receiver<ConfigChange>,
}

impl ConfigWatcher {
    /// Start watching the config directory.  The directory is created when it
    /// does not exist so a later manager save can be observed.
    pub fn start(path: PathBuf, hwnd: windows::Win32::Foundation::HWND) -> Option<Self> {
        let parent = path.parent()?.to_owned();
        let file_name = path.file_name()?.to_string_lossy().to_string();
        if std::fs::create_dir_all(&parent).is_err() {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        // HWND wraps a raw pointer and is not `Send`; pass its stable numeric
        // value to the watcher and reconstruct it inside the worker thread.
        let hwnd_value = hwnd.0 as isize;
        let thread = thread::Builder::new()
            .name("selection-translate-config-watch".to_owned())
            .spawn(move || {
                let hwnd = windows::Win32::Foundation::HWND(hwnd_value as *mut _);
                watch_directory(parent, file_name, path, hwnd, thread_stop, sender)
            })
            .ok()?;
        Some(Self {
            stop,
            thread: Some(thread),
            receiver,
        })
    }

    pub fn try_recv(&self) -> Result<ConfigChange, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // GetOverlappedResultEx is bounded to 500 ms, so the thread observes
        // the signal quickly even when no file changes are occurring.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch_directory(
    parent: PathBuf,
    file_name: String,
    path: PathBuf,
    hwnd: windows::Win32::Foundation::HWND,
    stop: Arc<AtomicBool>,
    sender: mpsc::Sender<ConfigChange>,
) {
    let parent_wide: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
    let directory = unsafe {
        CreateFileW(
            PCWSTR(parent_wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OVERLAPPED.0),
            None,
        )
    };
    let Ok(directory) = directory else {
        return;
    };

    let mut buffer = vec![0_u8; 16 * 1024];
    let incomplete_code = ERROR_IO_INCOMPLETE.to_hresult();
    let pending_code = ERROR_IO_PENDING.to_hresult();
    let timeout_code = windows::core::HRESULT::from_win32(WAIT_TIMEOUT.0);
    while !stop.load(Ordering::Acquire) {
        let mut overlapped = OVERLAPPED::default();
        let request = unsafe {
            ReadDirectoryChangesW(
                directory,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
                None,
                Some(&mut overlapped),
                None,
            )
        };
        if let Err(error) = request {
            if error.code() != pending_code {
                break;
            }
        }

        let mut transferred = 0_u32;
        let completed = loop {
            match unsafe {
                GetOverlappedResultEx(directory, &overlapped, &mut transferred, 500, false)
            } {
                Ok(()) => break true,
                // Keep waiting on this same OVERLAPPED request.  A timeout
                // does not complete or cancel ReadDirectoryChangesW, so the
                // request must not be issued again on the same handle.
                Err(error) if error.code() == incomplete_code || error.code() == timeout_code => {
                    if stop.load(Ordering::Acquire) {
                        cancel_overlapped(directory, &overlapped);
                        break false;
                    }
                }
                Err(_) => {
                    cancel_overlapped(directory, &overlapped);
                    break false;
                }
            }
        };
        if !completed {
            continue;
        }
        let transferred = (transferred as usize).min(buffer.len());
        if notification_mentions_file(&buffer[..transferred], &file_name) {
            // Atomic replacement may produce the rename notification before
            // the destination is readable.  A short debounce lets the writer
            // finish while still keeping reload effectively immediate.
            thread::sleep(Duration::from_millis(30));
            let change = match AppConfig::load(&path) {
                Ok(config) => ConfigChange::Loaded(config),
                Err(_) => ConfigChange::Invalid,
            };
            if sender.send(change).is_ok() {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(hwnd),
                        CONFIG_RELOAD_MESSAGE,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
            } else {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(directory) };
}

/// Wait for the cancelled overlapped operation to finish before its storage
/// goes out of scope and can be reused by the next directory read.
fn cancel_overlapped(directory: windows::Win32::Foundation::HANDLE, overlapped: &OVERLAPPED) {
    unsafe {
        let _ = windows::Win32::System::IO::CancelIoEx(directory, Some(overlapped));
        let mut transferred = 0_u32;
        loop {
            match GetOverlappedResultEx(directory, overlapped, &mut transferred, 500, false) {
                Ok(()) => break,
                Err(error) if error.code() == ERROR_OPERATION_ABORTED.to_hresult() => break,
                Err(error)
                    if error.code() == ERROR_IO_INCOMPLETE.to_hresult()
                        || error.code() == windows::core::HRESULT::from_win32(WAIT_TIMEOUT.0) =>
                {
                    continue
                }
                Err(_) => break,
            }
        }
    }
}

fn notification_mentions_file(buffer: &[u8], wanted: &str) -> bool {
    let mut offset = 0_usize;
    while offset + 12 <= buffer.len() {
        let next = u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
        let action = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
        let name_bytes =
            u32::from_ne_bytes(buffer[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let name_start = offset + 12;
        let name_end = name_start.saturating_add(name_bytes);
        if name_end > buffer.len() || !name_bytes.is_multiple_of(2) {
            break;
        }
        let name = String::from_utf16_lossy(
            &buffer[name_start..name_end]
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
        if name.eq_ignore_ascii_case(wanted)
            && matches!(
                action,
                x if x == FILE_ACTION_ADDED.0
                    || x == FILE_ACTION_MODIFIED.0
                    || x == FILE_ACTION_REMOVED.0
                    || x == FILE_ACTION_RENAMED_NEW_NAME.0
            )
        {
            return true;
        }
        if next == 0 {
            break;
        }
        if next < 12 || offset.saturating_add(next) <= offset {
            break;
        }
        offset += next;
    }
    false
}

#[cfg(not(windows))]
pub const CONFIG_RELOAD_MESSAGE: u32 = 0;

#[cfg(not(windows))]
pub enum ConfigChange {}

#[cfg(not(windows))]
pub struct ConfigWatcher;

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(action: u32, name: &str, next: u32) -> Vec<u8> {
        let encoded: Vec<u16> = name.encode_utf16().collect();
        let record_len = 12 + encoded.len() * 2;
        let mut bytes = vec![0_u8; record_len];
        bytes[0..4].copy_from_slice(&next.to_ne_bytes());
        bytes[4..8].copy_from_slice(&action.to_ne_bytes());
        bytes[8..12].copy_from_slice(&(encoded.len() as u32 * 2).to_ne_bytes());
        for (index, value) in encoded.iter().enumerate() {
            bytes[12 + index * 2..14 + index * 2].copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn notification_parser_matches_only_config_file_changes() {
        let changed = notification(FILE_ACTION_MODIFIED.0, "config.toml", 0);
        assert!(notification_mentions_file(&changed, "config.toml"));

        let unrelated = notification(FILE_ACTION_MODIFIED.0, "other.toml", 0);
        assert!(!notification_mentions_file(&unrelated, "config.toml"));

        let added = notification(FILE_ACTION_ADDED.0, "CONFIG.TOML", 0);
        assert!(notification_mentions_file(&added, "config.toml"));
    }

    #[test]
    fn notification_parser_rejects_truncated_name() {
        let mut malformed = notification(FILE_ACTION_MODIFIED.0, "config.toml", 0);
        malformed[8..12].copy_from_slice(&200_u32.to_ne_bytes());
        assert!(!notification_mentions_file(&malformed, "config.toml"));
    }

    #[test]
    fn watcher_publishes_only_a_valid_atomic_replacement() {
        let directory =
            std::env::temp_dir().join(format!("selection-translate-watch-{}", std::process::id()));
        let path = directory.join("config.toml");
        let watcher = ConfigWatcher::start(
            path.clone(),
            windows::Win32::Foundation::HWND(std::ptr::null_mut()),
        )
        .expect("watcher starts");
        // Regression guard: the first overlapped wait must time out while the
        // watch remains armed; a later save must still produce an event.
        thread::sleep(Duration::from_millis(800));
        selection_core::save_atomic(&path, &AppConfig::default()).expect("atomic config save");
        let event = watcher
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("watcher event");
        assert!(matches!(event, ConfigChange::Loaded(_)));
        drop(watcher);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(directory);
    }
}
