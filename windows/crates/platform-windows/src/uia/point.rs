//! Hover UI Automation extraction from the pointer coordinate.

use selection_core::{normalize::normalize_target, ExtractionSource, TextContext};
use selection_platform_interface::{ExtractionFailure, ExtractionResult, ScreenPoint};

/// Testable representation of one UI Automation bounding rectangle.
///
/// UI Automation returns these as `(left, top, width, height)` doubles.  The
/// helper below deliberately keeps the geometry independent of Win32 so the
/// containment rules can be tested without a live UI Automation provider.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
struct BoundingRect {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

#[allow(dead_code)]
fn point_is_inside_bounding_rectangles(point: (f64, f64), rectangles: &[f64]) -> bool {
    rectangles.chunks_exact(4).any(|values| {
        let rect = BoundingRect {
            left: values[0],
            top: values[1],
            width: values[2],
            height: values[3],
        };
        let right = rect.left + rect.width;
        let bottom = rect.top + rect.height;
        rect.left.is_finite()
            && rect.top.is_finite()
            && rect.width.is_finite()
            && rect.height.is_finite()
            && right.is_finite()
            && bottom.is_finite()
            && rect.width > 0.0
            && rect.height > 0.0
            && point.0.is_finite()
            && point.1.is_finite()
            && point.0 >= rect.left
            && point.0 < right
            && point.1 >= rect.top
            && point.1 < bottom
    })
}

#[cfg(windows)]
use selection_core::normalize::normalize_optional;
#[cfg(windows)]
use windows::Win32::Foundation::POINT;
#[cfg(windows)]
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, TextUnit_Line,
    TextUnit_Paragraph, TextUnit_Word, UIA_TextPatternId,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, WindowFromPoint, GA_ROOT};

#[cfg(windows)]
const MAX_TARGET_CHARS: i32 = super::context::MAX_UIA_TEXT_UNITS;
#[cfg(windows)]
const MAX_ANCESTOR_DEPTH: usize = 16;
#[cfg(windows)]
const MAX_ROOT_WALK_DEPTH: usize = 64;

#[cfg(windows)]
pub(crate) fn extract(
    automation: &IUIAutomation,
    process_id: u32,
    source_root_window: isize,
    pointer: ScreenPoint,
) -> ExtractionResult {
    if !pointer_root_matches(pointer, source_root_window) {
        crate::runtime_trace::record("hover_uia_native_root_mismatch");
        return Err(ExtractionFailure::StaleElement);
    }
    let mut element = unsafe {
        automation.ElementFromPoint(POINT {
            x: pointer.x,
            y: pointer.y,
        })
    }
    .map_err(|error| super::worker::map_error(&error))?;
    crate::runtime_trace::record("hover_uia_root_resolve_begin");
    let root = super::selection::resolve_root(automation, source_root_window)?;
    crate::runtime_trace::record("hover_uia_root_resolve_ok");
    let walker =
        unsafe { automation.RawViewWalker() }.map_err(|error| super::worker::map_error(&error))?;
    if let Some(root) = root.as_ref() {
        if !element_under_root(&element, root, automation, &walker)? {
            crate::runtime_trace::record("hover_uia_root_mismatch");
            return Err(ExtractionFailure::StaleElement);
        }
        crate::runtime_trace::record("hover_uia_root_check_ok");
    }
    if root.is_none() && process_id != 0 {
        let actual = super::selection::element_process_id(&element)?;
        if !super::selection::process_id_allowed(process_id, actual) {
            return Err(ExtractionFailure::StaleElement);
        }
    }
    // ElementFromPoint can return a raw-view text descendant which is absent
    // from the control view (WPF does this for text glyph peers). The bounded
    // raw walk reaches the nearest editor/provider without jumping directly
    // to an unrelated control.
    let mut last_failure = ExtractionFailure::UnsupportedPattern;
    for depth in 0..=MAX_ANCESTOR_DEPTH {
        crate::runtime_trace::record_id("hover_uia_ancestor_depth", depth as u64);
        match extract_from_element(&element, pointer) {
            Ok(result) if pointer_root_matches(pointer, source_root_window) => return Ok(result),
            Ok(_) => return Err(ExtractionFailure::StaleElement),
            Err(error) if can_try_ancestor(error) => last_failure = error,
            Err(error) => return Err(error),
        }
        if let Some(root) = root.as_ref() {
            let same = unsafe { automation.CompareElements(&element, root) }
                .map_err(|error| super::worker::map_error(&error))?;
            if same.as_bool() {
                break;
            }
        }
        if depth == MAX_ANCESTOR_DEPTH {
            break;
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&element) }) else {
            break;
        };
        if root.is_none() && process_id != 0 {
            let actual = super::selection::element_process_id(&parent)?;
            if !super::selection::process_id_allowed(process_id, actual) {
                break;
            }
        }
        element = parent;
    }
    Err(last_failure)
}

#[cfg(windows)]
fn element_under_root(
    element: &IUIAutomationElement,
    root: &IUIAutomationElement,
    automation: &IUIAutomation,
    walker: &windows::Win32::UI::Accessibility::IUIAutomationTreeWalker,
) -> Result<bool, ExtractionFailure> {
    let mut candidate = element.clone();
    for _ in 0..=MAX_ROOT_WALK_DEPTH {
        let same = unsafe { automation.CompareElements(&candidate, root) }
            .map_err(|error| super::worker::map_error(&error))?;
        if same.as_bool() {
            return Ok(true);
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&candidate) }) else {
            return Ok(false);
        };
        candidate = parent;
    }
    Ok(false)
}

#[cfg(windows)]
fn pointer_root_matches(pointer: ScreenPoint, expected_root: isize) -> bool {
    if expected_root == 0 {
        return true;
    }
    let pointed = unsafe {
        WindowFromPoint(POINT {
            x: pointer.x,
            y: pointer.y,
        })
    };
    if pointed.0.is_null() {
        return false;
    }
    let root = unsafe { GetAncestor(pointed, GA_ROOT) };
    let actual = if root.0.is_null() { pointed } else { root };
    actual.0 as isize == expected_root
}

#[cfg(windows)]
fn can_try_ancestor(error: ExtractionFailure) -> bool {
    matches!(
        error,
        ExtractionFailure::UnsupportedPattern | ExtractionFailure::EmptyRange
    )
}

#[cfg(windows)]
fn extract_from_element(element: &IUIAutomationElement, pointer: ScreenPoint) -> ExtractionResult {
    let pattern =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
            .map_err(|_| ExtractionFailure::UnsupportedPattern)?;
    crate::runtime_trace::record("hover_uia_text_pattern_ok");
    let range = unsafe {
        pattern.RangeFromPoint(POINT {
            x: pointer.x,
            y: pointer.y,
        })
    }
    .map_err(|error| super::worker::map_error(&error))?;
    crate::runtime_trace::record("hover_uia_range_from_point_ok");
    // Force the provider to validate that the range remains attached. The
    // returned enclosure may legitimately be a descendant of the ancestor
    // which exposes TextPattern, so identity/subtree walking is not an
    // acceptance condition. Native-root revalidation plus exact bounding-box
    // containment below bind the result to the unchanged pointer instead.
    let _enclosing =
        unsafe { range.GetEnclosingElement() }.map_err(|error| super::worker::map_error(&error))?;
    crate::runtime_trace::record("hover_uia_range_enclosing_ok");
    let word = unsafe { range.Clone() }.map_err(|error| super::worker::map_error(&error))?;
    unsafe { word.ExpandToEnclosingUnit(TextUnit_Word) }
        .map_err(|error| super::worker::map_error(&error))?;
    let Some(screen_rect) = word_containing_rect(&word, pointer) else {
        // RangeFromPoint is permitted to snap an outside/blank coordinate to
        // nearby text.  Treat that as a local miss so OCR can try the point.
        crate::runtime_trace::record("hover_uia_word_rect_miss");
        return Err(ExtractionFailure::EmptyRange);
    };
    let target = unsafe { word.GetText(MAX_TARGET_CHARS) }
        .map(|text| normalize_target(&text.to_string()))
        .map_err(|error| super::worker::map_error(&error))?;
    if target.is_empty() {
        crate::runtime_trace::record("hover_uia_target_empty");
        return Err(ExtractionFailure::EmptyRange);
    }

    let context = admit_hover_context(bounded_context(&word, &target));
    Ok(TextContext {
        target,
        context: context
            .as_deref()
            .and_then(|value| normalize_optional(Some(value))),
        source: ExtractionSource::UiaPoint,
        screen_rect: Some(screen_rect),
    })
}

#[cfg(windows)]
fn word_containing_rect(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
    pointer: ScreenPoint,
) -> Option<selection_core::ScreenRect> {
    let array = unsafe { range.GetBoundingRectangles() }.ok()?;
    if array.is_null() {
        crate::runtime_trace::record("hover_uia_rect_array_null");
        return None;
    }
    unsafe {
        use std::ffi::c_void;
        use std::ptr::null_mut;
        use std::slice;
        use windows::Win32::System::Ole::{
            SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElemsize,
            SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayGetVartype, SafeArrayUnaccessData,
        };
        use windows::Win32::System::Variant::VT_R8;
        let bounds = SafeArrayGetLBound(array, 1)
            .ok()
            .zip(SafeArrayGetUBound(array, 1).ok());
        let count = bounds
            .and_then(|(lower, upper)| {
                (upper >= lower)
                    .then(|| usize::try_from(i64::from(upper) - i64::from(lower) + 1).ok())
                    .flatten()
            })
            .unwrap_or(0);
        let valid = SafeArrayGetDim(array) == 1
            && count > 0
            && count <= 4096
            && count.is_multiple_of(4)
            && SafeArrayGetElemsize(array) as usize == std::mem::size_of::<f64>()
            && SafeArrayGetVartype(array).is_ok_and(|value| value == VT_R8);
        crate::runtime_trace::record_id("hover_uia_rect_value_count", count as u64);
        if !valid {
            crate::runtime_trace::record("hover_uia_rect_array_invalid");
        }
        let mut data: *mut c_void = null_mut();
        let result = if valid && SafeArrayAccessData(array, &mut data).is_ok() {
            let rect = if data.is_null() {
                None
            } else {
                let values = slice::from_raw_parts(data.cast::<f64>(), count);
                let point = (f64::from(pointer.x), f64::from(pointer.y));
                let containing = values.chunks_exact(4).find_map(|v| {
                    let (left, top, width, height) = (v[0], v[1], v[2], v[3]);
                    let right = left + width;
                    let bottom = top + height;
                    if ![left, top, width, height, right, bottom]
                        .iter()
                        .all(|v| v.is_finite())
                        || width <= 0.0
                        || height <= 0.0
                        || point.0 < left
                        || point.0 >= right
                        || point.1 < top
                        || point.1 >= bottom
                        || left < f64::from(i32::MIN)
                        || top < f64::from(i32::MIN)
                        || right > f64::from(i32::MAX)
                        || bottom > f64::from(i32::MAX)
                    {
                        return None;
                    }
                    let left = left.floor();
                    let top = top.floor();
                    let right = right.ceil();
                    let bottom = bottom.ceil();
                    if left < f64::from(i32::MIN)
                        || top < f64::from(i32::MIN)
                        || right > f64::from(i32::MAX)
                        || bottom > f64::from(i32::MAX)
                        || right <= left
                        || bottom <= top
                    {
                        return None;
                    }
                    Some(selection_core::ScreenRect::new(
                        left as i32,
                        top as i32,
                        right as i32,
                        bottom as i32,
                    ))
                });
                if containing.is_none() {
                    crate::runtime_trace::record("hover_uia_rect_no_containment");
                }
                containing
            };
            let _ = SafeArrayUnaccessData(array);
            rect
        } else {
            None
        };
        let _ = SafeArrayDestroy(array as *const _);
        result
    }
}

/// Admit only context that the UIA path actually derived from a paragraph or
/// line range.  `None`/formatting-only context is a recoverable local failure;
/// callers can then try OCR.  A one-word sentence is valid when UIA really
/// returned it, so equality with the target is intentionally allowed.
pub(crate) fn admit_hover_context(context: Option<String>) -> Option<String> {
    context.filter(|value| !normalize_target(value).is_empty())
}

#[cfg(windows)]
fn bounded_context(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
    target: &str,
) -> Option<String> {
    super::context::sentence_context_for_range(range, TextUnit_Paragraph, target)
        .or_else(|| super::context::sentence_context_for_range(range, TextUnit_Line, target))
        .filter(|text| !normalize_target(text).is_empty())
}

#[cfg(test)]
mod tests {
    use super::{admit_hover_context, can_try_ancestor, point_is_inside_bounding_rectangles};
    use selection_platform_interface::ExtractionFailure;

    #[test]
    fn point_containment_uses_half_open_rectangle_boundaries() {
        let rectangle = [10.0, 20.0, 30.0, 40.0];
        assert!(point_is_inside_bounding_rectangles(
            (10.0, 20.0),
            &rectangle
        ));
        assert!(point_is_inside_bounding_rectangles(
            (39.999, 59.999),
            &rectangle
        ));
        assert!(!point_is_inside_bounding_rectangles(
            (40.0, 30.0),
            &rectangle
        ));
        assert!(!point_is_inside_bounding_rectangles(
            (30.0, 60.0),
            &rectangle
        ));
    }

    #[test]
    fn point_containment_accepts_any_matching_rectangle() {
        let rectangles = [0.0, 0.0, 2.0, 2.0, 100.0, 200.0, 20.0, 10.0];
        assert!(point_is_inside_bounding_rectangles(
            (110.0, 205.0),
            &rectangles
        ));
        assert!(!point_is_inside_bounding_rectangles(
            (50.0, 50.0),
            &rectangles
        ));
    }

    #[test]
    fn point_containment_rejects_malformed_or_nonfinite_rectangles() {
        assert!(!point_is_inside_bounding_rectangles(
            (1.0, 1.0),
            &[0.0, 0.0, f64::NAN, 4.0]
        ));
        assert!(!point_is_inside_bounding_rectangles(
            (1.0, 1.0),
            &[0.0, 0.0, -2.0, 4.0]
        ));
        assert!(!point_is_inside_bounding_rectangles(
            (f64::INFINITY, 1.0),
            &[0.0, 0.0, 4.0, 4.0]
        ));
        assert!(!point_is_inside_bounding_rectangles(
            (1.0, 1.0),
            &[0.0, 0.0, 4.0]
        ));
    }

    #[test]
    fn hover_context_requires_derived_nonempty_text() {
        assert_eq!(admit_hover_context(None), None);
        assert_eq!(admit_hover_context(Some("\u{200B} \u{FEFF}".into())), None);
        assert_eq!(
            admit_hover_context(Some("word".into())),
            Some("word".into())
        );
        assert_eq!(
            admit_hover_context(Some("A whole sentence.".into())),
            Some("A whole sentence.".into())
        );
    }

    #[test]
    fn ancestor_fallback_is_limited_to_local_pattern_or_range_misses() {
        assert!(can_try_ancestor(ExtractionFailure::UnsupportedPattern));
        assert!(can_try_ancestor(ExtractionFailure::EmptyRange));
        assert!(!can_try_ancestor(ExtractionFailure::StaleElement));
        assert!(!can_try_ancestor(ExtractionFailure::PermissionDenied));
        assert!(!can_try_ancestor(ExtractionFailure::Platform));
    }
}
