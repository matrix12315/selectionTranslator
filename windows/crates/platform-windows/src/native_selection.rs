//! Bounded native Edit/RichEdit selection extraction.
//!
//! This is deliberately a fallback for controls whose UI Automation provider
//! does not expose TextPattern. It prefers the focused control owned by the
//! originating process, then falls back to the window under the mouse-up point.

use selection_core::{sentence::sentence_for_target_at, ExtractionSource, ScreenRect, TextContext};
use selection_platform_interface::{
    CancellationToken, ExtractionFailure, ExtractionResult, ScreenPoint,
};

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsWindowUnicode, SendMessageTimeoutW, WindowFromPoint, GA_PARENT,
    GUITHREADINFO, GWL_STYLE, SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT, WM_GETTEXT,
    WM_GETTEXTLENGTH,
};

const EM_GETSEL: u32 = 0x00B0;
const EM_GETPASSWORDCHAR: u32 = 0x00D2;
const ES_PASSWORD: isize = 0x20;

const MAX_PARENT_DEPTH: usize = 8;
const MAX_TEXT_UNITS: usize = 32 * 1024;
const MESSAGE_TIMEOUT_MS: u32 = 80;

/// Extract the current selection from a focused or pointed native
/// Edit/RichEdit control. The user-facing source remains `UiaSelection`
/// because this is one Selection extraction pipeline, not a distinct trigger.
#[cfg(windows)]
pub(crate) fn extract_cancellable(
    pointer: Option<ScreenPoint>,
    process_id: u32,
    selection_rect: Option<ScreenRect>,
    cancellation: &CancellationToken,
) -> ExtractionResult {
    if cancellation.is_cancelled() {
        return Err(ExtractionFailure::Platform);
    }
    if !native_fallback_allowed(selection_rect) {
        return Err(ExtractionFailure::UnsupportedPattern);
    }
    let point = pointer.ok_or(ExtractionFailure::UnsupportedPattern)?;
    if process_id == 0 || process_id == std::process::id() {
        return Err(ExtractionFailure::UnsupportedPattern);
    }
    let point_origin = unsafe {
        WindowFromPoint(POINT {
            x: point.x,
            y: point.y,
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    // A low-level mouse hook reports the mouse-up point asynchronously. By
    // the time this worker handles it, WindowFromPoint can resolve a parent
    // or overlay rather than the edit control which owns the active range.
    // The foreground thread's focused control is a stronger identity for a
    // selection and remains constrained to the originating process. Keep the
    // point path as a fallback for controls which do not expose focus through
    // GUI thread information.
    let hwnd = focused_edit_for_process(process_id, deadline)
        .or_else(|| {
            (!point_origin.0.is_null())
                .then(|| find_edit_ancestor(point_origin, process_id, deadline).ok())
                .flatten()
        })
        .ok_or(ExtractionFailure::UnsupportedPattern)?;
    if cancellation.is_cancelled() {
        return Err(ExtractionFailure::Platform);
    }

    let mut start = 0u32;
    let mut end = 0u32;
    let first_range = get_selection(hwnd, &mut start, &mut end, deadline)?;
    if !valid_utf16_range(start, end, MAX_TEXT_UNITS) || start == end {
        return Err(ExtractionFailure::EmptyRange);
    }
    if cancellation.is_cancelled() {
        return Err(ExtractionFailure::Platform);
    }
    let text = read_window_text(hwnd, deadline)?;
    validate_candidate(hwnd, process_id, deadline)?;
    let units: Vec<u16> = text.encode_utf16().collect();
    if end as usize > units.len() {
        return Err(ExtractionFailure::StaleElement);
    }
    let mut second_start = 0;
    let mut second_end = 0;
    get_selection(hwnd, &mut second_start, &mut second_end, deadline)?;
    if first_range != (second_start, second_end) {
        return Err(ExtractionFailure::StaleElement);
    }
    let raw_target = String::from_utf16(&units[start as usize..end as usize])
        .map_err(|_| ExtractionFailure::StaleElement)?;
    let target = selection_core::normalize::normalize_target(&raw_target);
    if target.is_empty() {
        return Err(ExtractionFailure::EmptyRange);
    }
    if target.chars().count() > selection_core::request_gate::MAX_TARGET_SCALARS {
        return Err(ExtractionFailure::Platform);
    }
    let byte_offset =
        utf16_offset_to_byte(&units, start as usize).ok_or(ExtractionFailure::StaleElement)?;
    let full_text = String::from_utf16(&units).map_err(|_| ExtractionFailure::StaleElement)?;
    let context = derive_context(&full_text, byte_offset, &target);
    Ok(TextContext {
        target,
        context,
        source: ExtractionSource::UiaSelection,
        screen_rect: selection_rect,
    })
}

#[cfg(windows)]
fn focused_edit_for_process(expected_pid: u32, deadline: std::time::Instant) -> Option<HWND> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    let mut foreground_pid = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_pid)) };
    if thread_id == 0 || foreground_pid != expected_pid {
        return None;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(thread_id, &mut info) }.is_err() {
        return None;
    }
    let focused = info.hwndFocus;
    if focused.0.is_null() {
        return None;
    }
    find_edit_ancestor(focused, expected_pid, deadline).ok()
}

#[cfg(not(windows))]
pub(crate) fn extract_cancellable(
    _pointer: Option<ScreenPoint>,
    _process_id: u32,
    _selection_rect: Option<ScreenRect>,
    _cancellation: &CancellationToken,
) -> ExtractionResult {
    Err(ExtractionFailure::UnsupportedPattern)
}

#[cfg(windows)]
fn find_edit_ancestor(
    mut hwnd: HWND,
    expected_pid: u32,
    deadline: std::time::Instant,
) -> Result<HWND, ExtractionFailure> {
    for _ in 0..=MAX_PARENT_DEPTH {
        if hwnd.0.is_null() {
            break;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != expected_pid {
            return Err(ExtractionFailure::StaleElement);
        }
        if is_allowed_edit_class(hwnd) {
            check_text_window_policy(hwnd, deadline)?;
            return Ok(hwnd);
        }
        hwnd = unsafe { GetAncestor(hwnd, GA_PARENT) };
    }
    Err(ExtractionFailure::UnsupportedPattern)
}

#[cfg(windows)]
fn is_allowed_edit_class(hwnd: HWND) -> bool {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) } as usize;
    if length == 0 || length >= buffer.len() {
        return false;
    }
    let class = String::from_utf16_lossy(&buffer[..length]);
    is_allowed_edit_class_name(&class)
}

#[cfg(windows)]
fn check_text_window_policy(
    hwnd: HWND,
    deadline: std::time::Instant,
) -> Result<(), ExtractionFailure> {
    if !unsafe { IsWindowUnicode(hwnd) }.as_bool() {
        return Err(ExtractionFailure::UnsupportedPattern);
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    if style & ES_PASSWORD != 0 {
        return Err(ExtractionFailure::PermissionDenied);
    }
    let result = send_message(hwnd, EM_GETPASSWORDCHAR, WPARAM(0), LPARAM(0), deadline)
        .map_err(|_| ExtractionFailure::PermissionDenied)?;
    if result != 0 {
        Err(ExtractionFailure::PermissionDenied)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn validate_candidate(
    hwnd: HWND,
    expected_pid: u32,
    deadline: std::time::Instant,
) -> Result<(), ExtractionFailure> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != expected_pid || !is_allowed_edit_class(hwnd) {
        return Err(ExtractionFailure::StaleElement);
    }
    check_text_window_policy(hwnd, deadline)
}

fn is_allowed_edit_class_name(class: &str) -> bool {
    let lower = class.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "edit" | "richedit20w" | "richedit50w" | "richeditd2dpt"
    ) || lower.starts_with("windowsforms10.edit.")
        || lower.starts_with("windowsforms10.richedit20w.")
        || lower.starts_with("windowsforms10.richedit50w.")
}

fn valid_utf16_range(start: u32, end: u32, max_units: usize) -> bool {
    let start = start as usize;
    let end = end as usize;
    start <= end && end <= max_units
}

fn utf16_offset_to_byte(units: &[u16], offset: usize) -> Option<usize> {
    if offset > units.len() {
        return None;
    }
    String::from_utf16(&units[..offset])
        .ok()
        .map(|text| text.len())
}

fn derive_context(full_text: &str, byte_offset: usize, target: &str) -> Option<String> {
    let adjusted_offset = full_text[byte_offset..]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace() && !matches!(ch, '\u{200B}' | '\u{2060}' | '\u{FEFF}'))
        .map_or(byte_offset, |(offset, _)| byte_offset + offset);
    sentence_for_target_at(full_text, target, adjusted_offset)
        .map(|value| selection_core::normalize::normalize_text(&value))
        .filter(|value| value != target && !value.is_empty())
}

fn native_fallback_allowed(selection_rect: Option<ScreenRect>) -> bool {
    selection_rect.is_some()
}

#[cfg(windows)]
fn get_selection(
    hwnd: HWND,
    start: &mut u32,
    end: &mut u32,
    deadline: std::time::Instant,
) -> Result<(u32, u32), ExtractionFailure> {
    send_message(
        hwnd,
        EM_GETSEL,
        WPARAM((start as *mut u32) as usize),
        LPARAM((end as *mut u32) as isize),
        deadline,
    )?;
    Ok((*start, *end))
}

#[cfg(windows)]
fn send_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    deadline: std::time::Instant,
) -> Result<usize, ExtractionFailure> {
    let remaining = deadline
        .checked_duration_since(std::time::Instant::now())
        .ok_or(ExtractionFailure::Platform)?;
    let timeout = (remaining.as_millis() as u32).clamp(1, MESSAGE_TIMEOUT_MS);
    let mut result = 0usize;
    let status = unsafe {
        SendMessageTimeoutW(
            hwnd,
            message,
            wparam,
            lparam,
            SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
            timeout,
            Some(&mut result as *mut usize),
        )
    };
    if status.0 == 0 {
        Err(ExtractionFailure::Platform)
    } else {
        Ok(result)
    }
}

#[cfg(windows)]
fn read_window_text(hwnd: HWND, deadline: std::time::Instant) -> Result<String, ExtractionFailure> {
    let length = send_message(hwnd, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0), deadline)?;
    if length > MAX_TEXT_UNITS {
        return Err(ExtractionFailure::Platform);
    }
    if length == 0 {
        return Err(ExtractionFailure::EmptyRange);
    }
    let mut buffer = vec![0u16; length.checked_add(1).ok_or(ExtractionFailure::Platform)?];
    let copied = send_message(
        hwnd,
        WM_GETTEXT,
        WPARAM(buffer.len()),
        LPARAM(buffer.as_mut_ptr() as isize),
        deadline,
    )?;
    if copied > length {
        return Err(ExtractionFailure::StaleElement);
    }
    if buffer[..copied].contains(&0) {
        return Err(ExtractionFailure::StaleElement);
    }
    String::from_utf16(&buffer[..copied]).map_err(|_| ExtractionFailure::StaleElement)
}

#[cfg(test)]
mod tests {
    use super::{
        derive_context, is_allowed_edit_class_name, native_fallback_allowed, utf16_offset_to_byte,
        valid_utf16_range,
    };
    use selection_core::ScreenRect;

    #[test]
    fn allowlist_is_narrow_and_unicode_only() {
        assert!(is_allowed_edit_class_name("Edit"));
        assert!(is_allowed_edit_class_name("RichEdit20W"));
        assert!(is_allowed_edit_class_name("RICHEDIT50W"));
        assert!(is_allowed_edit_class_name("richeditd2dpt"));
        assert!(is_allowed_edit_class_name("WindowsForms10.EDIT.abcdef"));
        assert!(!is_allowed_edit_class_name("Button"));
        assert!(is_allowed_edit_class_name("edit"));
        assert!(!is_allowed_edit_class_name("RichEdit20A"));
    }

    #[test]
    fn ranges_are_bounded_and_ordered() {
        assert!(valid_utf16_range(1, 2, 8));
        assert!(!valid_utf16_range(2, 1, 8));
        assert!(!valid_utf16_range(0, 9, 8));
    }

    #[test]
    fn utf16_offsets_handle_non_bmp() {
        let units: Vec<u16> = "a😀b".encode_utf16().collect();
        assert_eq!(utf16_offset_to_byte(&units, 0), Some(0));
        assert_eq!(utf16_offset_to_byte(&units, 1), Some(1));
        assert_eq!(utf16_offset_to_byte(&units, 3), Some(5));
        assert_eq!(utf16_offset_to_byte(&units, 4), Some(6));
    }

    #[test]
    fn invalid_surrogate_prefix_is_rejected() {
        assert_eq!(utf16_offset_to_byte(&[0xD800], 1), None);
    }

    #[test]
    fn automatic_native_fallback_requires_selection_geometry() {
        assert!(!native_fallback_allowed(None));
        assert!(native_fallback_allowed(Some(ScreenRect::new(1, 2, 3, 4))));
    }

    #[test]
    fn context_handles_repeated_targets_leading_space_and_crlf() {
        let text = "First word.\r\n  word second.";
        let offset = text.rfind("  word").unwrap();
        assert_eq!(
            derive_context(text, offset, "word"),
            Some("word second.".into())
        );
    }

    #[test]
    fn context_handles_zero_width_and_non_bmp() {
        let text = "One. \u{200B}😀 target here.";
        let offset = text.find('\u{200B}').unwrap();
        assert_eq!(
            derive_context(text, offset, "😀"),
            Some("😀 target here.".into())
        );
    }
}
