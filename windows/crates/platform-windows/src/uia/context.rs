//! Bounded UI Automation context retrieval anchored to a real text range.
//!
//! `GetText` on an expanded paragraph starts at the paragraph's beginning.
//! Reading only a fixed prefix can therefore omit a word near the end and
//! make repeated-word matching ambiguous. This module creates a bounded
//! window around the actual UIA range and returns the target's UTF-8 byte
//! offset inside that window.

use selection_core::sentence::sentence_for_target_at;

#[cfg(windows)]
use windows::Win32::UI::Accessibility::{
    IUIAutomationTextRange, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, TextUnit,
    TextUnit_Character,
};

/// Keep enough UTF-16 capacity to retrieve a 4,000-scalar target/context even
/// when every scalar is represented by a surrogate pair. The request gate is
/// still the authority for the 4,000-scalar target limit.
#[cfg(windows)]
pub(crate) const MAX_UIA_TEXT_UNITS: i32 = 16_001;

#[cfg(windows)]
const CONTEXT_HALF_UNITS: i32 = 2_048;

#[cfg(windows)]
pub(crate) fn context_for_range(
    range: &IUIAutomationTextRange,
    expand_unit: TextUnit,
) -> Option<(String, usize)> {
    let expanded = unsafe { range.Clone().ok()? };
    unsafe { expanded.ExpandToEnclosingUnit(expand_unit).ok()? };

    // Start with the enclosing unit, move the start to the selected range,
    // then make a bounded window around that exact range.
    let window = unsafe { expanded.Clone().ok()? };
    unsafe {
        window
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                range,
                TextPatternRangeEndpoint_Start,
            )
            .ok()?;
        window
            .MoveEndpointByUnit(
                TextPatternRangeEndpoint_Start,
                TextUnit_Character,
                -CONTEXT_HALF_UNITS,
            )
            .ok()?;
    }

    // The prefix is a separate range because it gives us an unambiguous
    // occurrence offset even if the same word appears many times.
    let prefix = unsafe { window.Clone().ok()? };
    unsafe {
        prefix
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                range,
                TextPatternRangeEndpoint_Start,
            )
            .ok()?;
    }
    let prefix_text = unsafe { prefix.GetText(MAX_UIA_TEXT_UNITS).ok()?.to_string() };
    let target_offset_scalars = prefix_text.chars().count();

    unsafe {
        window
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                range,
                TextPatternRangeEndpoint_End,
            )
            .ok()?;
        window
            .MoveEndpointByUnit(
                TextPatternRangeEndpoint_End,
                TextUnit_Character,
                CONTEXT_HALF_UNITS,
            )
            .ok()?;
    }
    let text = unsafe { window.GetText(MAX_UIA_TEXT_UNITS).ok()?.to_string() };
    let target_offset = text
        .char_indices()
        .nth(target_offset_scalars)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    Some((text, target_offset))
}

#[cfg(windows)]
pub(crate) fn sentence_context_for_range(
    range: &IUIAutomationTextRange,
    expand_unit: TextUnit,
    target: &str,
) -> Option<String> {
    let (context, target_offset) = context_for_range(range, expand_unit)?;
    sentence_for_target_at(&context, target, target_offset)
}
