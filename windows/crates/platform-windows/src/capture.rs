//! Small, in-memory screen capture used only as the final OCR fallback.
//!
//! The capture owns every GDI handle it creates and never writes an image to
//! disk.  The public geometry helpers are kept independent of Win32 so the
//! important edge cases can be tested on every host.

use selection_core::ScreenRect;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedPixels {
    pub rect: ScreenRect,
    pub width: u32,
    pub height: u32,
    /// 32-bit BGRA pixels, tightly packed, top-to-bottom.
    pub bgra: Vec<u8>,
}

pub const MAX_CAPTURE_DIMENSION: u32 = 8_192;
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

pub fn checked_capture_len(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_CAPTURE_DIMENSION || height > MAX_CAPTURE_DIMENSION
    {
        return None;
    }
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    (len <= MAX_CAPTURE_BYTES).then_some(len)
}

pub fn inflate_rect(rect: ScreenRect, padding: i32) -> ScreenRect {
    ScreenRect::new(
        rect.left.saturating_sub(padding),
        rect.top.saturating_sub(padding),
        rect.right.saturating_add(padding),
        rect.bottom.saturating_add(padding),
    )
}

pub fn clamp_rect(rect: ScreenRect, bounds: ScreenRect) -> Option<ScreenRect> {
    let left = rect.left.min(rect.right).max(bounds.left);
    let top = rect.top.min(rect.bottom).max(bounds.top);
    let right = rect.left.max(rect.right).min(bounds.right);
    let bottom = rect.top.max(rect.bottom).min(bounds.bottom);
    (right > left && bottom > top).then(|| ScreenRect::new(left, top, right, bottom))
}

#[cfg(windows)]
pub fn nearest_work_area(point: selection_platform_interface::ScreenPoint) -> ScreenRect {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !monitor.is_invalid() && GetMonitorInfoW(monitor, &mut info).as_bool() {
            return rect_from_win32(info.rcMonitor);
        }
    }
    ScreenRect::new(i32::MIN / 2, i32::MIN / 2, i32::MAX / 2, i32::MAX / 2)
}

/// Return the effective DPI of the monitor containing `point`.
///
/// This deliberately queries the target monitor instead of the process/system
/// default. The resident is per-monitor-DPI-aware, so a crop around a pointer
/// on a secondary monitor must be scaled using that monitor's DPI.
#[cfg(windows)]
pub fn monitor_dpi(point: selection_platform_interface::ScreenPoint) -> u32 {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        if !monitor.is_invalid() {
            let mut x = 96;
            let mut y = 96;
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() {
                return x.max(96);
            }
        }
    }
    96
}

#[cfg(windows)]
fn virtual_screen_bounds() -> ScreenRect {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    ScreenRect::new(
        left,
        top,
        left.saturating_add(width),
        top.saturating_add(height),
    )
}

#[cfg(windows)]
fn rect_from_win32(rect: windows::Win32::Foundation::RECT) -> ScreenRect {
    ScreenRect::new(rect.left, rect.top, rect.right, rect.bottom)
}

#[cfg(windows)]
pub fn capture(
    rect: ScreenRect,
) -> Result<CapturedPixels, selection_platform_interface::ExtractionFailure> {
    use std::ffi::c_void;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ, SRCCOPY,
    };

    // Use the complete virtual desktop as the capture safety boundary. A crop
    // may legitimately cross monitors; clamping to the monitor containing its
    // top-left would silently lose the other side of the selected sentence.
    #[cfg(windows)]
    let Some(rect) = clamp_rect(rect, virtual_screen_bounds()) else {
        return Err(selection_platform_interface::ExtractionFailure::EmptyRange);
    };
    let width_i = rect.right.saturating_sub(rect.left);
    let height_i = rect.bottom.saturating_sub(rect.top);
    let (width, height) = (u32::try_from(width_i).ok(), u32::try_from(height_i).ok());
    let (Some(width), Some(height)) = (width, height) else {
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    };
    let Some(buffer_len) = checked_capture_len(width, height) else {
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    };
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    }
    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    if memory.is_invalid() {
        unsafe { ReleaseDC(None, screen) };
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width as i32, height as i32) };
    if bitmap.is_invalid() {
        unsafe {
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    }
    let old = unsafe { SelectObject(memory, HGDIOBJ(bitmap.0)) };
    if old.0.is_null() || old.0 as isize == -1 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    }
    let mut pixels = vec![0u8; buffer_len];
    let copied = unsafe {
        BitBlt(
            memory,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            rect.left,
            rect.top,
            SRCCOPY,
        )
    }
    .is_ok();
    // GetDIBits requires the destination bitmap to be deselected from the DC.
    // Restore the previous object before reading, then keep cleanup idempotent.
    let restored = unsafe { SelectObject(memory, old) };
    if restored.0.is_null() || restored.0 as isize == -1 {
        unsafe {
            // If restoration failed, the bitmap may still be selected. Drop
            // the memory DC first so DeleteObject cannot fail for that reason.
            let _ = DeleteDC(memory);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            ReleaseDC(None, screen);
        }
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let read = copied
        && unsafe {
            GetDIBits(
                memory,
                bitmap,
                0,
                height,
                Some(pixels.as_mut_ptr().cast::<c_void>()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } == height as i32;
    unsafe {
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
    }
    if !read {
        return Err(selection_platform_interface::ExtractionFailure::Platform);
    }
    Ok(CapturedPixels {
        rect,
        width,
        height,
        bgra: pixels,
    })
}

#[cfg(not(windows))]
pub fn capture(
    _rect: ScreenRect,
) -> Result<CapturedPixels, selection_platform_interface::ExtractionFailure> {
    Err(selection_platform_interface::ExtractionFailure::Platform)
}

#[cfg(test)]
mod tests {
    use super::{checked_capture_len, clamp_rect, inflate_rect};
    use selection_core::ScreenRect;

    #[test]
    fn inflate_and_clamp_handles_negative_virtual_coordinates() {
        let inflated = inflate_rect(ScreenRect::new(-10, -5, 10, 5), 16);
        assert_eq!(inflated, ScreenRect::new(-26, -21, 26, 21));
        assert_eq!(
            clamp_rect(inflated, ScreenRect::new(-20, -10, 20, 10)),
            Some(ScreenRect::new(-20, -10, 20, 10))
        );
    }

    #[test]
    fn empty_intersection_is_rejected() {
        assert_eq!(
            clamp_rect(ScreenRect::new(0, 0, 1, 1), ScreenRect::new(2, 2, 3, 3)),
            None
        );
    }

    #[test]
    fn cross_monitor_capture_is_clamped_to_the_virtual_desktop_union() {
        let virtual_desktop = ScreenRect::new(-1920, 0, 3840, 2160);
        assert_eq!(
            clamp_rect(ScreenRect::new(-100, 100, 100, 300), virtual_desktop),
            Some(ScreenRect::new(-100, 100, 100, 300))
        );
    }

    #[test]
    fn capture_allocation_is_checked_and_bounded() {
        assert_eq!(checked_capture_len(2, 3), Some(24));
        assert_eq!(checked_capture_len(0, 1), None);
        assert_eq!(checked_capture_len(8_193, 1), None);
        assert_eq!(checked_capture_len(8_192, 8_192), None);
    }
}
