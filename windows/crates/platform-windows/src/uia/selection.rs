//! Focused-element UI Automation selection extraction.

use selection_core::{ExtractionSource, ScreenRect, TextContext};
use selection_platform_interface::{ExtractionFailure, ExtractionResult, ScreenPoint};

#[cfg(windows)]
use selection_core::normalize::{normalize_optional, normalize_target};
#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(windows)]
use windows::Win32::Foundation::POINT;
#[cfg(windows)]
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTextPattern2,
    TextUnit_Line, TextUnit_Paragraph, UIA_TextPattern2Id, UIA_TextPatternId,
};

#[cfg(windows)]
const MAX_TARGET_CHARS: i32 = super::context::MAX_UIA_TEXT_UNITS;
#[cfg(windows)]
const MAX_EXTRACTION_DEPTH: usize = 32;
#[cfg(windows)]
const MAX_ROOT_WALK_DEPTH: usize = 64;
#[cfg(windows)]
const _: () = assert!(MAX_EXTRACTION_DEPTH <= MAX_ROOT_WALK_DEPTH);

#[cfg(windows)]
pub(crate) fn extract(
    automation: &IUIAutomation,
    process_id: u32,
    source_root_window: isize,
    pointer: Option<ScreenPoint>,
    selection_rect: Option<ScreenRect>,
) -> ExtractionResult {
    let focused = unsafe { automation.GetFocusedElement() }
        .map_err(|error| super::worker::map_error(&error))?;
    crate::runtime_trace::record("uia_focused_candidate");
    let root = resolve_root(automation, source_root_window)?;

    // The focused element is often a leaf inside a composite editor. Walk a
    // small, same-process ancestor chain so a child with an empty or partial
    // provider does not prevent its editor from supplying the selection.
    match extract_chain(
        automation,
        focused,
        process_id,
        root.as_ref(),
        selection_rect,
    ) {
        Ok(result) => Ok(result),
        Err(error) if !can_try_point_fallback(error) => Err(error),
        Err(focused_failure) => {
            // Some controls expose a useful TextPattern only from the element
            // under the selection point. This is still bounded and remains
            // constrained to the originating process.
            let Some(pointer) = pointer else {
                return Err(focused_failure);
            };
            crate::runtime_trace::record("uia_point_fallback_candidate");
            let point_element = unsafe {
                automation.ElementFromPoint(POINT {
                    x: pointer.x,
                    y: pointer.y,
                })
            }
            .map_err(|error| super::worker::map_error(&error))?;
            extract_chain(
                automation,
                point_element,
                process_id,
                root.as_ref(),
                selection_rect,
            )
        }
    }
}

#[cfg(windows)]
fn extract_chain(
    automation: &IUIAutomation,
    mut element: IUIAutomationElement,
    process_id: u32,
    root: Option<&IUIAutomationElement>,
    selection_rect: Option<ScreenRect>,
) -> ExtractionResult {
    let walker = unsafe { automation.ControlViewWalker() }.ok();
    let mut last_failure = ExtractionFailure::UnsupportedPattern;
    if let Some(root) = root {
        if process_id != 0 {
            let actual = element_process_id(&element)?;
            if actual != process_id {
                crate::runtime_trace::record_id("uia_renderer_pid", u64::from(actual));
            }
        }
        if !element_under_root(automation, &element, root)? {
            crate::runtime_trace::record("uia_root_mismatch");
            return Err(ExtractionFailure::StaleElement);
        }
    }
    for depth in 0..=MAX_EXTRACTION_DEPTH {
        if root.is_none() && !process_id_allowed(process_id, element_process_id(&element)?) {
            return Err(ExtractionFailure::StaleElement);
        }

        match extract_from_element(&element, selection_rect) {
            Ok(result) => return Ok(result),
            Err(error) if can_try_ancestor(error) => last_failure = error,
            Err(error) => return Err(error),
        }

        if let Some(root) = root {
            let same = unsafe { automation.CompareElements(&element, root) }
                .map_err(|error| super::worker::map_error(&error))?;
            if same.as_bool() {
                break;
            }
        }

        if depth == MAX_EXTRACTION_DEPTH {
            break;
        }
        let Some(tree) = walker.as_ref() else {
            break;
        };
        let Ok(parent) = (unsafe { tree.GetParentElement(&element) }) else {
            break;
        };
        element = parent;
    }
    Err(last_failure)
}

#[cfg(windows)]
pub(super) fn resolve_root(
    automation: &IUIAutomation,
    source_root_window: isize,
) -> Result<Option<IUIAutomationElement>, ExtractionFailure> {
    if source_root_window == 0 {
        return Ok(None);
    }
    let root = unsafe { automation.ElementFromHandle(HWND(source_root_window as *mut _)) }
        .map_err(|error| super::worker::map_error(&error))?;
    Ok(Some(root))
}

#[cfg(windows)]
fn element_under_root(
    automation: &IUIAutomation,
    element: &IUIAutomationElement,
    root: &IUIAutomationElement,
) -> Result<bool, ExtractionFailure> {
    let walker = unsafe { automation.ControlViewWalker() }
        .map_err(|error| super::worker::map_error(&error))?;
    let mut candidate = element.clone();
    for _ in 0..=MAX_ROOT_WALK_DEPTH {
        let same = unsafe { automation.CompareElements(&candidate, root) }
            .map_err(|error| super::worker::map_error(&error))?;
        if same.as_bool() {
            return Ok(true);
        }
        let parent = unsafe { walker.GetParentElement(&candidate) }
            .map_err(|error| super::worker::map_error(&error))?;
        candidate = parent;
    }
    Ok(false)
}

#[cfg(windows)]
pub(super) fn element_process_id(element: &IUIAutomationElement) -> Result<u32, ExtractionFailure> {
    let process_id =
        unsafe { element.CurrentProcessId() }.map_err(|error| super::worker::map_error(&error))?;
    u32::try_from(process_id).map_err(|_| ExtractionFailure::StaleElement)
}

#[cfg(windows)]
pub(super) fn process_id_allowed(expected: u32, actual: u32) -> bool {
    let allowed = expected == 0 || expected == actual;
    if !allowed {
        // Numeric process identities are privacy-safe and make multiprocess
        // UIA provider mismatches diagnosable without recording window text,
        // element names, selected content, or executable paths.
        crate::runtime_trace::record_id("uia_process_expected", u64::from(expected));
        crate::runtime_trace::record_id("uia_process_actual", u64::from(actual));
    }
    allowed
}

#[cfg(windows)]
fn can_try_ancestor(error: ExtractionFailure) -> bool {
    matches!(
        error,
        ExtractionFailure::UnsupportedPattern | ExtractionFailure::EmptyRange
    )
}

#[cfg(windows)]
fn can_try_point_fallback(error: ExtractionFailure) -> bool {
    matches!(
        error,
        ExtractionFailure::UnsupportedPattern
            | ExtractionFailure::EmptyRange
            | ExtractionFailure::StaleElement
    )
}

#[cfg(windows)]
fn extract_from_element(
    element: &IUIAutomationElement,
    selection_rect: Option<ScreenRect>,
) -> ExtractionResult {
    // TextPattern2 is preferred where available, but it inherits all of the
    // selection methods from TextPattern. The fallback covers older controls.
    let pattern = unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id)
            .map(Pattern::V2)
            .or_else(|_| {
                element
                    .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    .map(Pattern::V1)
            })
    }
    .map_err(|_| ExtractionFailure::UnsupportedPattern)?;

    let ranges = pattern.selection()?;
    let length = unsafe { ranges.Length() }
        .map_err(|error| super::worker::map_error(&error))?
        .clamp(0, 64);
    if length == 0 {
        return Err(ExtractionFailure::EmptyRange);
    }

    let mut targets = Vec::new();
    let mut context = None;
    for index in 0..length {
        let range = unsafe { ranges.GetElement(index) }
            .map_err(|error| super::worker::map_error(&error))?;
        let target = get_text(&range, MAX_TARGET_CHARS)?;
        let target = normalize_target(&target);
        if target.is_empty() {
            continue;
        }
        if context.is_none() {
            context = context_for_range(&range, &target);
        }
        targets.push(target);
    }
    if targets.is_empty() {
        return Err(ExtractionFailure::EmptyRange);
    }

    let target = targets.join("\n");
    let context = normalize_optional(context.as_deref()).filter(|value| value != &target);
    Ok(TextContext {
        target,
        context,
        source: ExtractionSource::UiaSelection,
        screen_rect: selection_rect,
    })
}

#[cfg(windows)]
enum Pattern {
    V1(IUIAutomationTextPattern),
    V2(IUIAutomationTextPattern2),
}

#[cfg(windows)]
impl Pattern {
    fn selection(
        &self,
    ) -> Result<windows::Win32::UI::Accessibility::IUIAutomationTextRangeArray, ExtractionFailure>
    {
        unsafe {
            match self {
                Self::V1(pattern) => pattern
                    .GetSelection()
                    .map_err(|error| super::worker::map_error(&error)),
                Self::V2(pattern) => pattern
                    .GetSelection()
                    .map_err(|error| super::worker::map_error(&error)),
            }
        }
    }
}

#[cfg(windows)]
fn get_text(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
    max_chars: i32,
) -> Result<String, ExtractionFailure> {
    unsafe { range.GetText(max_chars) }
        .map(|text| text.to_string())
        .map_err(|error| super::worker::map_error(&error))
}

#[cfg(windows)]
fn context_for_range(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
    target: &str,
) -> Option<String> {
    super::context::sentence_context_for_range(range, TextUnit_Paragraph, target)
        .or_else(|| super::context::sentence_context_for_range(range, TextUnit_Line, target))
        .filter(|text| !normalize_target(text).is_empty())
        .or_else(|| Some(target.to_owned()))
}

#[cfg(all(test, windows))]
mod tests {
    use super::{can_try_point_fallback, process_id_allowed};
    use selection_platform_interface::ExtractionFailure;

    #[test]
    fn zero_expected_process_id_is_unconstrained() {
        assert!(process_id_allowed(0, 1));
        assert!(process_id_allowed(0, u32::MAX));
    }

    #[test]
    fn nonzero_expected_process_id_requires_an_exact_match() {
        assert!(process_id_allowed(42, 42));
        assert!(!process_id_allowed(42, 43));
        assert!(!process_id_allowed(42, 0));
    }

    #[test]
    fn point_fallback_accepts_recoverable_focused_failures_only() {
        assert!(can_try_point_fallback(
            ExtractionFailure::UnsupportedPattern
        ));
        assert!(can_try_point_fallback(ExtractionFailure::EmptyRange));
        assert!(can_try_point_fallback(ExtractionFailure::StaleElement));
        assert!(!can_try_point_fallback(ExtractionFailure::PermissionDenied));
        assert!(!can_try_point_fallback(ExtractionFailure::Platform));
    }
}
