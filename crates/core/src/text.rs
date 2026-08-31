//! Text and trigger values shared by extractors and the resident coordinator.

/// The event that caused extraction to be attempted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TriggerKind {
    Selection,
    Manual,
    Hover,
}

/// The extraction path that produced a text context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ExtractionSource {
    UiaSelection,
    UiaPoint,
    Clipboard,
    Ocr,
}

/// A rectangle in screen coordinates. Coordinates are intentionally signed:
/// a monitor may be positioned to the left or above the primary monitor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// Target text and optional local context returned by an extractor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextContext {
    pub target: String,
    pub context: Option<String>,
    pub source: ExtractionSource,
    pub screen_rect: Option<ScreenRect>,
}

impl TextContext {
    pub fn new(target: impl Into<String>, source: ExtractionSource) -> Self {
        Self {
            target: target.into(),
            context: None,
            source,
            screen_rect: None,
        }
    }
}
