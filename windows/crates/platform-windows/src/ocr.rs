//! Windows Runtime OCR adapter.  It is deliberately a fallback: callers can
//! use UIA/clipboard first and instantiate this extractor only when needed.

use selection_core::{
    normalize::normalize_target, ExtractionSource, ScreenRect, TextContext, TriggerKind,
};
use selection_platform_interface::{
    CancellationToken, ExtractionFailure, ExtractionResult, ScreenPoint, TextExtractor,
};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::capture::{capture, inflate_rect};

/// Windows system OCR rejects images whose largest dimension exceeds this
/// limit. Keep the check local and deterministic rather than relying on an
/// asynchronous WinRT failure.
pub const MAX_OCR_IMAGE_DIMENSION: u32 = 2048;

#[derive(Clone, Debug, PartialEq)]
pub struct WordBox {
    pub text: String,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl WordBox {
    fn distance_squared(&self, x: f32, y: f32) -> f32 {
        let dx = if x < self.left {
            self.left - x
        } else if x > self.right {
            x - self.right
        } else {
            0.0
        };
        let dy = if y < self.top {
            self.top - y
        } else if y > self.bottom {
            y - self.bottom
        } else {
            0.0
        };
        dx * dx + dy * dy
    }
}

pub fn nearest_word(words: &[WordBox], x: f32, y: f32) -> Option<usize> {
    words
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            left.distance_squared(x, y)
                .partial_cmp(&right.distance_squared(x, y))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
}

/// Return the OCR word directly under a hover pointer.
///
/// Hover is intentionally stricter than manual OCR: a nearby OCR result is
/// not evidence that the pointer is over text.  OCR providers can also emit
/// malformed rectangles, so those are excluded before doing the hit test.
fn hover_word(words: &[WordBox], x: f32, y: f32) -> Option<usize> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }

    words.iter().enumerate().find_map(|(index, word)| {
        let valid = word.left.is_finite()
            && word.top.is_finite()
            && word.right.is_finite()
            && word.bottom.is_finite()
            && word.left < word.right
            && word.top < word.bottom;
        let inside = valid && x >= word.left && x < word.right && y >= word.top && y < word.bottom;
        inside.then_some(index)
    })
}

/// OCR is isolated on one worker because WinRT async completion can depend on
/// external language services. A blocked operation must not hold the resident
/// extraction dispatcher or process shutdown indefinitely.
pub struct OcrExtractor {
    worker: Mutex<Option<JoinHandle<()>>>,
}

const OCR_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const WAIT_SLICE: Duration = Duration::from_millis(20);
const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_millis(100);
const MANUAL_POINTER_WIDTH: i32 = 960;
const MANUAL_POINTER_HEIGHT: i32 = 320;
const HOVER_CONTEXT_WIDTH: i32 = 1_536;
const HOVER_CONTEXT_HEIGHT: i32 = 384;

impl OcrExtractor {
    pub const fn new() -> Self {
        Self {
            worker: Mutex::new(None),
        }
    }

    pub fn selection_rect(rect: ScreenRect) -> ScreenRect {
        inflate_rect(rect, 16)
    }

    /// Expand a selected region enough to include the surrounding sentence.
    /// The target region itself is still passed separately to OCR word
    /// intersection, so this expansion cannot cause unrelated words to be
    /// admitted as the selected target.
    pub fn selection_context_rect(rect: ScreenRect) -> ScreenRect {
        #[cfg(windows)]
        let dpi = {
            let center_x = (i64::from(rect.left) + i64::from(rect.right)) / 2;
            let center_y = (i64::from(rect.top) + i64::from(rect.bottom)) / 2;
            let point = ScreenPoint::new(
                center_x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                center_y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            );
            crate::capture::monitor_dpi(point).max(96)
        };
        #[cfg(not(windows))]
        let dpi = 96u32;
        selection_context_rect_for_dpi(rect, dpi)
    }

    pub fn manual_rect(selection_rect: Option<ScreenRect>, pointer: ScreenPoint) -> ScreenRect {
        selection_rect
            .map(Self::selection_context_rect)
            .unwrap_or_else(|| {
                #[cfg(windows)]
                {
                    pointer_rect_for_dpi(
                        pointer,
                        MANUAL_POINTER_WIDTH,
                        MANUAL_POINTER_HEIGHT,
                        crate::capture::monitor_dpi(pointer),
                    )
                }
                #[cfg(not(windows))]
                pointer_rect_for_dpi(pointer, MANUAL_POINTER_WIDTH, MANUAL_POINTER_HEIGHT, 96)
            })
    }

    pub fn hover_rect(pointer: ScreenPoint) -> ScreenRect {
        #[cfg(windows)]
        let dpi = crate::capture::monitor_dpi(pointer);
        #[cfg(not(windows))]
        let dpi = 96;
        pointer_rect_for_dpi(pointer, HOVER_CONTEXT_WIDTH, HOVER_CONTEXT_HEIGHT, dpi)
    }

    pub(crate) fn extract_cancellable(
        &self,
        trigger: TriggerKind,
        pointer: Option<ScreenPoint>,
        selection_rect: Option<ScreenRect>,
        cancellation: &CancellationToken,
    ) -> ExtractionResult {
        if cancellation.is_cancelled() {
            return Err(ExtractionFailure::Platform);
        }
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        {
            let mut worker = self
                .worker
                .lock()
                .map_err(|_| ExtractionFailure::Platform)?;
            if let Some(previous) = worker.take() {
                if previous.is_finished() {
                    let _ = previous.join();
                } else {
                    *worker = Some(previous);
                    // One quarantined WinRT operation is the hard limit; do
                    // not create a thread leak when Windows OCR is unhealthy.
                    return Err(ExtractionFailure::Platform);
                }
            }
            let next = thread::Builder::new()
                .name("selection-translate-ocr".to_owned())
                .spawn(move || {
                    let _ = response_tx.send(extract_once(trigger, pointer, selection_rect));
                })
                .map_err(|_| ExtractionFailure::Platform)?;
            *worker = Some(next);
        }
        let deadline = Instant::now() + OCR_REQUEST_TIMEOUT;
        loop {
            if cancellation.is_cancelled() {
                return Err(ExtractionFailure::Platform);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ExtractionFailure::Platform);
            };
            match response_rx.recv_timeout(remaining.min(WAIT_SLICE)) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ExtractionFailure::Platform);
                }
            }
        }
    }
}

fn pointer_rect_for_dpi(
    pointer: ScreenPoint,
    logical_width: i32,
    logical_height: i32,
    dpi: u32,
) -> ScreenRect {
    let dpi = u64::from(dpi.max(96));
    let scale = |logical: i32| {
        (u64::try_from(logical.max(1))
            .unwrap_or(1)
            .saturating_mul(dpi)
            / 96)
            .min(i32::MAX as u64) as i32
    };
    let width = scale(logical_width);
    let height = scale(logical_height);
    ScreenRect::new(
        pointer.x.saturating_sub(width / 2),
        pointer.y.saturating_sub(height / 2),
        pointer.x.saturating_add(width / 2),
        pointer.y.saturating_add(height / 2),
    )
}

/// Return bounded, DPI-scaled context padding.  Horizontal padding is wide
/// enough to include the rest of a sentence; vertical padding remains shallow
/// so a selection does not trigger an unnecessarily large OCR capture.
pub(crate) fn selection_context_padding(dpi: u32) -> (i32, i32) {
    let dpi = dpi.max(96);
    let scale = u64::from(dpi);
    let horizontal = (480u64.saturating_mul(scale) / 96).min(1_536) as i32;
    let vertical = (48u64.saturating_mul(scale) / 96).min(256) as i32;
    (horizontal, vertical)
}

pub(crate) fn selection_context_rect_for_dpi(rect: ScreenRect, dpi: u32) -> ScreenRect {
    let (horizontal, vertical) = selection_context_padding(dpi);
    inflate_rect_xy(rect, horizontal, vertical)
}

fn inflate_rect_xy(rect: ScreenRect, horizontal: i32, vertical: i32) -> ScreenRect {
    ScreenRect::new(
        rect.left.saturating_sub(horizontal),
        rect.top.saturating_sub(vertical),
        rect.right.saturating_add(horizontal),
        rect.bottom.saturating_add(vertical),
    )
}

impl Default for OcrExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl TextExtractor for OcrExtractor {
    fn extract(
        &self,
        trigger: TriggerKind,
        pointer: Option<ScreenPoint>,
        selection_rect: Option<ScreenRect>,
    ) -> ExtractionResult {
        self.extract_cancellable(trigger, pointer, selection_rect, &CancellationToken::new())
    }
}

impl Drop for OcrExtractor {
    fn drop(&mut self) {
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(worker) = worker.take() {
                let deadline = Instant::now() + SHUTDOWN_JOIN_BUDGET;
                while !worker.is_finished() && Instant::now() < deadline {
                    thread::sleep(WAIT_SLICE.min(SHUTDOWN_JOIN_BUDGET));
                }
                if worker.is_finished() {
                    let _ = worker.join();
                }
            }
        }
    }
}

fn extract_once(
    trigger: TriggerKind,
    pointer: Option<ScreenPoint>,
    selection_rect: Option<ScreenRect>,
) -> ExtractionResult {
    let rect = match trigger {
        TriggerKind::Selection => selection_rect
            .map(OcrExtractor::selection_context_rect)
            .ok_or(ExtractionFailure::EmptyRange)?,
        TriggerKind::Manual => OcrExtractor::manual_rect(
            selection_rect,
            pointer.ok_or(ExtractionFailure::EmptyRange)?,
        ),
        TriggerKind::Hover => {
            OcrExtractor::hover_rect(pointer.ok_or(ExtractionFailure::EmptyRange)?)
        }
    };
    crate::runtime_trace::record("ocr_capture_begin");
    let pixels = match capture(rect) {
        Ok(pixels) => {
            crate::runtime_trace::record("ocr_capture_ok");
            pixels
        }
        Err(error) => {
            crate::runtime_trace::record("ocr_capture_failure");
            return Err(error);
        }
    };
    recognize(pixels, trigger, pointer, selection_rect)
}

/// Return the exact number of bytes required by a tightly packed BGRA8 image.
///
/// The checked arithmetic is intentional: dimensions originate at the Win32
/// capture boundary and must never be cast or multiplied unchecked before a
/// WinRT buffer is constructed.
pub(crate) fn expected_bgra8_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

pub(crate) fn has_exact_bgra8_len(width: u32, height: u32, actual: usize) -> bool {
    expected_bgra8_len(width, height) == Some(actual)
        && width > 0
        && height > 0
        && width <= i32::MAX as u32
        && height <= i32::MAX as u32
}

#[cfg(windows)]
fn recognize(
    pixels: crate::capture::CapturedPixels,
    trigger: TriggerKind,
    pointer: Option<ScreenPoint>,
    selection_rect: Option<ScreenRect>,
) -> ExtractionResult {
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;
    use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|_| {
        crate::runtime_trace::record("ocr_ro_initialize_failure");
        ExtractionFailure::Platform
    })?;
    struct WinRtGuard;
    impl Drop for WinRtGuard {
        fn drop(&mut self) {
            unsafe { RoUninitialize() };
        }
    }
    let _winrt = WinRtGuard;

    if !has_exact_bgra8_len(pixels.width, pixels.height, pixels.bgra.len()) {
        crate::runtime_trace::record("ocr_buffer_length_failure");
        return Err(ExtractionFailure::Platform);
    }
    crate::runtime_trace::record("ocr_buffer_length_ok");
    let max_dimension = OcrEngine::MaxImageDimension().map_err(|_| {
        crate::runtime_trace::record("ocr_max_dimension_failure");
        ExtractionFailure::Platform
    })?;
    if pixels.width > max_dimension || pixels.height > max_dimension {
        crate::runtime_trace::record("ocr_image_dimension_failure");
        return Err(ExtractionFailure::Platform);
    }
    // DataWriter::new owns an in-memory buffer.  StoreAsync is deliberately
    // not called: it drains the writer into an output stream, after which
    // DetachBuffer returns an empty buffer.
    let writer = DataWriter::new().map_err(|_| {
        crate::runtime_trace::record("ocr_data_writer_failure");
        ExtractionFailure::Platform
    })?;
    writer.WriteBytes(&pixels.bgra).map_err(|_| {
        crate::runtime_trace::record("ocr_buffer_write_failure");
        ExtractionFailure::Platform
    })?;
    let buffer = writer.DetachBuffer().map_err(|_| {
        crate::runtime_trace::record("ocr_buffer_detach_failure");
        ExtractionFailure::Platform
    })?;
    let expected_len =
        expected_bgra8_len(pixels.width, pixels.height).ok_or(ExtractionFailure::Platform)?;
    let actual_len = buffer.Length().map_err(|_| {
        crate::runtime_trace::record("ocr_buffer_length_failure");
        ExtractionFailure::Platform
    })?;
    if usize::try_from(actual_len).ok() != Some(expected_len) {
        crate::runtime_trace::record("ocr_buffer_length_mismatch");
        return Err(ExtractionFailure::Platform);
    }
    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        pixels.width as i32,
        pixels.height as i32,
        BitmapAlphaMode::Ignore,
    )
    .map_err(|_| {
        crate::runtime_trace::record("ocr_bitmap_failure");
        ExtractionFailure::Platform
    })?;
    crate::runtime_trace::record("ocr_bitmap_ok");
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
        crate::runtime_trace::record("ocr_engine_unavailable");
        ExtractionFailure::UnsupportedPattern
    })?;
    crate::runtime_trace::record("ocr_engine_ready");
    let result = engine
        .RecognizeAsync(&bitmap)
        .map_err(|_| {
            crate::runtime_trace::record("ocr_recognition_failure");
            ExtractionFailure::Platform
        })?
        .get()
        .map_err(|_| {
            crate::runtime_trace::record("ocr_recognition_failure");
            ExtractionFailure::Platform
        })?;
    crate::runtime_trace::record("ocr_recognition_ok");
    let lines = result.Lines().map_err(|_| {
        crate::runtime_trace::record("ocr_recognition_failure");
        ExtractionFailure::Platform
    })?;
    let mut words = Vec::new();
    let mut word_offsets = Vec::new();
    let mut word_line_indices = Vec::new();
    let mut word_line_offsets = Vec::new();
    let mut context = String::new();
    let mut line_texts = Vec::new();
    let mut line_bounds = Vec::new();
    for line_index in 0..lines.Size().map_err(|_| ExtractionFailure::Platform)? {
        let line = lines
            .GetAt(line_index)
            .map_err(|_| ExtractionFailure::Platform)?;
        if !context.is_empty() {
            context.push(' ');
        }
        let line_context_start = context.len();
        let line_text = line
            .Text()
            .map_err(|_| ExtractionFailure::Platform)?
            .to_string();
        let mut line_search_start = 0usize;
        context.push_str(&line_text);
        let line_words = line.Words().map_err(|_| ExtractionFailure::Platform)?;
        let current_line_index = line_texts.len();
        let mut current_line_bounds: Option<(f32, f32, f32, f32)> = None;
        for word_index in 0..line_words.Size().map_err(|_| ExtractionFailure::Platform)? {
            let word = line_words
                .GetAt(word_index)
                .map_err(|_| ExtractionFailure::Platform)?;
            let bounds = word
                .BoundingRect()
                .map_err(|_| ExtractionFailure::Platform)?;
            let word_text = word
                .Text()
                .map_err(|_| ExtractionFailure::Platform)?
                .to_string();
            let line_offset = line_text[line_search_start..]
                .find(&word_text)
                .map(|relative| {
                    let start = line_search_start + relative;
                    line_search_start = start.saturating_add(word_text.len());
                    start
                });
            word_offsets.push(line_offset.map(|offset| line_context_start + offset));
            word_line_indices.push(current_line_index);
            word_line_offsets.push(line_offset);
            let word_bounds = (
                bounds.X,
                bounds.Y,
                bounds.X + bounds.Width,
                bounds.Y + bounds.Height,
            );
            if valid_line_bounds(word_bounds) {
                current_line_bounds = Some(match current_line_bounds {
                    Some(existing) => (
                        existing.0.min(word_bounds.0),
                        existing.1.min(word_bounds.1),
                        existing.2.max(word_bounds.2),
                        existing.3.max(word_bounds.3),
                    ),
                    None => word_bounds,
                });
            }
            words.push(WordBox {
                text: word_text,
                left: bounds.X,
                top: bounds.Y,
                right: bounds.X + bounds.Width,
                bottom: bounds.Y + bounds.Height,
            });
        }
        line_texts.push(line_text);
        line_bounds.push(current_line_bounds.unwrap_or((f32::NAN, 0.0, 0.0, 0.0)));
    }
    if words.is_empty() {
        crate::runtime_trace::record("ocr_no_words");
        return Err(ExtractionFailure::EmptyRange);
    }
    if context.trim().is_empty() {
        crate::runtime_trace::record("ocr_no_words");
        return Err(ExtractionFailure::EmptyRange);
    }
    let target_indices = match (trigger, selection_rect) {
        (TriggerKind::Selection, Some(region)) | (TriggerKind::Manual, Some(region)) => {
            word_indices_in_region(&words, pixels.rect, region)
        }
        (TriggerKind::Hover, _) => pointer
            .and_then(|point| {
                hover_word(
                    &words,
                    (point.x - pixels.rect.left) as f32,
                    (point.y - pixels.rect.top) as f32,
                )
            })
            .into_iter()
            .collect(),
        _ => pointer
            .and_then(|point| {
                nearest_word(
                    &words,
                    (point.x - pixels.rect.left) as f32,
                    (point.y - pixels.rect.top) as f32,
                )
            })
            .into_iter()
            .collect(),
    };
    // Normalize at the extractor boundary as well as at request admission.
    // OCR providers can return formatting-only tokens (for example a
    // zero-width character or whitespace).  Treat those as absent instead
    // of allowing an apparently non-empty TextContext to short-circuit the
    // fallback pipeline.
    let target = normalized_words_target(&words, &target_indices);
    if target.is_empty() {
        crate::runtime_trace::record("ocr_no_target");
        return Err(ExtractionFailure::EmptyRange);
    }
    let context = if trigger == TriggerKind::Hover {
        target_indices.first().and_then(|index| {
            hover_sentence_from_lines(
                &target,
                *index,
                &word_line_indices,
                &word_line_offsets,
                &line_texts,
                &line_bounds,
            )
        })
    } else {
        target_indices
            .first()
            .and_then(|index| word_offsets.get(*index).copied().flatten())
            .and_then(|offset| {
                selection_core::sentence::sentence_for_target_at(&context, &target, offset)
            })
    };
    let context = match trigger {
        TriggerKind::Hover => hover_context_admitted(&target, context),
        TriggerKind::Selection | TriggerKind::Manual => context,
    };
    if trigger == TriggerKind::Hover && context.is_none() {
        return Err(ExtractionFailure::EmptyRange);
    }
    Ok(TextContext {
        target,
        context,
        source: ExtractionSource::Ocr,
        screen_rect: target_screen_rect(&words, &target_indices, pixels.rect),
    })
}

fn target_screen_rect(
    words: &[WordBox],
    indices: &[usize],
    capture: ScreenRect,
) -> Option<ScreenRect> {
    let mut rect: Option<ScreenRect> = None;
    for &index in indices {
        let Some(word) = words.get(index) else {
            continue;
        };
        if !(word.left.is_finite()
            && word.top.is_finite()
            && word.right.is_finite()
            && word.bottom.is_finite())
        {
            continue;
        }
        let current = ScreenRect::new(
            capture.left.saturating_add(word.left.floor() as i32),
            capture.top.saturating_add(word.top.floor() as i32),
            capture.left.saturating_add(word.right.ceil() as i32),
            capture.top.saturating_add(word.bottom.ceil() as i32),
        );
        rect = Some(match rect {
            Some(existing) => ScreenRect::new(
                existing.left.min(current.left),
                existing.top.min(current.top),
                existing.right.max(current.right),
                existing.bottom.max(current.bottom),
            ),
            None => current,
        });
    }
    rect
}

fn normalized_words_target(words: &[WordBox], indices: &[usize]) -> String {
    indices
        .iter()
        .filter_map(|index| {
            words
                .get(*index)
                .map(|word| normalize_target(word.text.trim()))
                .filter(|word| !word.is_empty())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Hover OCR must return a sentence derived from the recognized text.  A
/// target-only result is rejected by the caller before it reaches the
/// provider; a genuinely one-word sentence is still valid.
fn hover_context_admitted(target: &str, context: Option<String>) -> Option<String> {
    context.filter(|value| {
        let normalized = normalize_target(value);
        !normalized.is_empty()
            && (normalized == normalize_target(target)
                || normalized.contains(&normalize_target(target)))
    })
}

fn hover_sentence_from_lines(
    target: &str,
    target_word_index: usize,
    word_line_indices: &[usize],
    word_line_offsets: &[Option<usize>],
    line_texts: &[String],
    line_bounds: &[(f32, f32, f32, f32)],
) -> Option<String> {
    let target_line = *word_line_indices.get(target_word_index)?;
    let target_offset_in_line = word_line_offsets
        .get(target_word_index)
        .copied()
        .flatten()?;
    let group = contiguous_line_group(line_bounds, target_line);
    if group.is_empty() {
        return None;
    }

    let mut source = String::new();
    let mut target_offset = None;
    for line_index in group {
        let line = line_texts.get(line_index)?;
        if !source.is_empty() {
            source.push(' ');
        }
        let line_start = source.len();
        if line_index == target_line {
            if target_offset_in_line > line.len() || !line.is_char_boundary(target_offset_in_line) {
                return None;
            }
            target_offset = Some(line_start + target_offset_in_line);
        }
        source.push_str(line);
    }
    selection_core::sentence::sentence_for_target_at(&source, target, target_offset?)
}

fn valid_line_bounds((left, top, right, bottom): (f32, f32, f32, f32)) -> bool {
    [left, top, right, bottom]
        .iter()
        .all(|value| value.is_finite())
        && right > left
        && bottom > top
}

fn lines_are_contiguous(first: (f32, f32, f32, f32), second: (f32, f32, f32, f32)) -> bool {
    if !valid_line_bounds(first) || !valid_line_bounds(second) {
        return false;
    }
    let (left, top, right, bottom) = first;
    let (other_left, other_top, other_right, other_bottom) = second;
    let vertical_gap = if other_bottom < top {
        top - other_bottom
    } else if other_top > bottom {
        other_top - bottom
    } else {
        0.0
    };
    let horizontal_gap = if other_right < left {
        left - other_right
    } else if other_left > right {
        other_left - right
    } else {
        0.0
    };
    let max_height = (bottom - top).max(other_bottom - other_top);
    vertical_gap <= max_height * 1.5 && horizontal_gap <= max_height * 2.0
}

/// Select the target OCR line plus wrapped lines that are geometrically
/// contiguous. Lines in unrelated crop columns are intentionally excluded.
pub(crate) fn contiguous_line_group(lines: &[(f32, f32, f32, f32)], target: usize) -> Vec<usize> {
    let Some(&target_bounds) = lines.get(target) else {
        return Vec::new();
    };
    if !valid_line_bounds(target_bounds) {
        return Vec::new();
    }
    let mut out = vec![target];
    let mut changed = true;
    while changed {
        changed = false;
        for (index, &bounds) in lines.iter().enumerate() {
            if out.contains(&index) || !valid_line_bounds(bounds) {
                continue;
            }
            if out
                .iter()
                .any(|&included| lines_are_contiguous(lines[included], bounds))
            {
                out.push(index);
                changed = true;
            }
        }
    }
    out.sort_unstable();
    out
}

#[cfg(windows)]
fn word_indices_in_region(
    words: &[WordBox],
    capture: ScreenRect,
    region: ScreenRect,
) -> Vec<usize> {
    words
        .iter()
        .enumerate()
        .filter(|word| {
            let (_, word) = word;
            let left = capture.left as f32 + word.left;
            let top = capture.top as f32 + word.top;
            let right = capture.left as f32 + word.right;
            let bottom = capture.top as f32 + word.bottom;
            right >= region.left as f32
                && left <= region.right as f32
                && bottom >= region.top as f32
                && top <= region.bottom as f32
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(not(windows))]
fn recognize(
    _pixels: crate::capture::CapturedPixels,
    _trigger: TriggerKind,
    _pointer: Option<ScreenPoint>,
    _selection_rect: Option<ScreenRect>,
) -> ExtractionResult {
    Err(ExtractionFailure::Platform)
}

#[cfg(test)]
mod tests {
    use super::{
        contiguous_line_group, expected_bgra8_len, has_exact_bgra8_len, hover_context_admitted,
        hover_sentence_from_lines, hover_word, nearest_word, normalized_words_target,
        pointer_rect_for_dpi, selection_context_padding, selection_context_rect_for_dpi,
        word_indices_in_region, OcrExtractor, WordBox, HOVER_CONTEXT_HEIGHT, HOVER_CONTEXT_WIDTH,
        MANUAL_POINTER_HEIGHT, MANUAL_POINTER_WIDTH,
    };
    use selection_core::{ScreenRect, TriggerKind};
    use selection_platform_interface::ScreenPoint;

    #[test]
    fn nearest_word_prefers_containing_or_closest_box() {
        let words = vec![
            WordBox {
                text: "one".into(),
                left: 0.0,
                top: 0.0,
                right: 20.0,
                bottom: 20.0,
            },
            WordBox {
                text: "two".into(),
                left: 30.0,
                top: 0.0,
                right: 50.0,
                bottom: 20.0,
            },
        ];
        assert_eq!(nearest_word(&words, 10.0, 10.0), Some(0));
        assert_eq!(nearest_word(&words, 49.0, 10.0), Some(1));
    }

    #[test]
    fn geometry_matches_trigger_contract() {
        let target = ScreenRect::new(10, 20, 30, 40);
        assert_eq!(
            OcrExtractor::selection_rect(target),
            ScreenRect::new(-6, 4, 46, 56)
        );
        assert_eq!(selection_context_padding(96), (480, 48));
        assert_eq!(
            selection_context_rect_for_dpi(target, 96),
            ScreenRect::new(-470, -28, 510, 88)
        );
        let context = selection_context_rect_for_dpi(target, 96);
        assert!(context.left < target.left);
        assert!(context.top < target.top);
        assert!(context.right > target.right);
        assert!(context.bottom > target.bottom);
        assert_eq!(
            OcrExtractor::manual_rect(None, ScreenPoint::new(0, 0)),
            ScreenRect::new(-480, -160, 480, 160)
        );
        assert_eq!(
            pointer_rect_for_dpi(
                ScreenPoint::new(0, 0),
                MANUAL_POINTER_WIDTH,
                MANUAL_POINTER_HEIGHT,
                192,
            ),
            ScreenRect::new(-960, -320, 960, 320)
        );
        assert_eq!(
            pointer_rect_for_dpi(
                ScreenPoint::new(0, 0),
                HOVER_CONTEXT_WIDTH,
                HOVER_CONTEXT_HEIGHT,
                96,
            ),
            ScreenRect::new(-768, -192, 768, 192)
        );
        assert_eq!(
            OcrExtractor::hover_rect(ScreenPoint::new(0, 0)),
            pointer_rect_for_dpi(
                ScreenPoint::new(0, 0),
                HOVER_CONTEXT_WIDTH,
                HOVER_CONTEXT_HEIGHT,
                crate::capture::monitor_dpi(ScreenPoint::new(0, 0)),
            )
        );
        let _ = TriggerKind::Hover;
    }

    #[test]
    fn region_selection_uses_the_recognized_word_index_for_repeated_words() {
        let words = vec![
            WordBox {
                text: "same".into(),
                left: 0.0,
                top: 0.0,
                right: 35.0,
                bottom: 20.0,
            },
            WordBox {
                text: "same".into(),
                left: 100.0,
                top: 0.0,
                right: 135.0,
                bottom: 20.0,
            },
        ];
        assert_eq!(
            word_indices_in_region(
                &words,
                ScreenRect::new(0, 0, 160, 40),
                ScreenRect::new(90, 0, 140, 25),
            ),
            vec![1]
        );
    }

    #[test]
    fn hover_requires_derived_sentence_context() {
        assert_eq!(hover_context_admitted("word", None), None);
        assert_eq!(
            hover_context_admitted("word", Some("\u{200B} \u{FEFF}".into())),
            None
        );
        assert_eq!(
            hover_context_admitted("word", Some("word".into())),
            Some("word".into())
        );
        assert_eq!(
            hover_context_admitted("word", Some("A word in context.".into())),
            Some("A word in context.".into())
        );
        assert_eq!(
            hover_context_admitted("word", Some("other sentence.".into())),
            None
        );
    }

    fn word(text: &str, left: f32, top: f32, right: f32, bottom: f32) -> WordBox {
        WordBox {
            text: text.into(),
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn hover_requires_pointer_inside_a_valid_word_box() {
        let words = vec![word("hello", 10.0, 10.0, 30.0, 25.0)];

        assert_eq!(hover_word(&words, 20.0, 20.0), Some(0));
        assert_eq!(hover_word(&words, 5.0, 20.0), None);
        assert_eq!(hover_word(&words, 30.0, 20.0), None);
        assert_eq!(hover_word(&words, f32::NAN, 20.0), None);
    }

    #[test]
    fn hover_skips_malformed_boxes_and_uses_the_exact_containing_word() {
        let words = vec![
            word("malformed", 40.0, 10.0, 20.0, 25.0),
            word("infinite", f32::NAN, 10.0, 60.0, 25.0),
            word("target", 10.0, 10.0, 30.0, 25.0),
            word("nearby", 31.0, 10.0, 50.0, 25.0),
        ];

        assert_eq!(hover_word(&words, 20.0, 20.0), Some(2));
        assert_eq!(hover_word(&words, 35.0, 20.0), Some(3));
        assert_eq!(hover_word(&words, 55.0, 20.0), None);
    }

    #[test]
    fn hover_line_group_keeps_wrapped_neighbors_and_rejects_far_columns() {
        let lines = vec![
            (10.0, 10.0, 100.0, 20.0),
            (12.0, 22.0, 95.0, 32.0),
            (11.0, 34.0, 98.0, 44.0),
            (300.0, 22.0, 390.0, 32.0),
        ];
        assert_eq!(contiguous_line_group(&lines, 0), vec![0, 1, 2]);
    }

    #[test]
    fn hover_sentence_joins_only_the_target_wrapped_line_group() {
        let lines = vec![
            "This sentence".to_owned(),
            "wraps around target here.".to_owned(),
            "Unrelated column.".to_owned(),
        ];
        let bounds = vec![
            (10.0, 10.0, 100.0, 20.0),
            (12.0, 22.0, 110.0, 32.0),
            (300.0, 22.0, 390.0, 32.0),
        ];
        assert_eq!(
            hover_sentence_from_lines(
                "target",
                0,
                &[1],
                &[Some("wraps around ".len())],
                &lines,
                &bounds,
            )
            .as_deref(),
            Some("This sentence wraps around target here.")
        );
    }

    #[test]
    fn formatting_only_ocr_words_are_absent_from_the_target() {
        let words = vec![
            word("\u{200B}", 0.0, 0.0, 10.0, 10.0),
            word("  \u{FEFF} ", 10.0, 0.0, 20.0, 10.0),
            word("actual", 20.0, 0.0, 40.0, 10.0),
        ];
        assert_eq!(normalized_words_target(&words, &[0, 1]), "");
        assert_eq!(normalized_words_target(&words, &[0, 2]), "actual");
    }

    #[test]
    fn bgra8_length_is_checked_without_overflow() {
        assert_eq!(expected_bgra8_len(2, 3), Some(24));
        assert_eq!(
            expected_bgra8_len(u32::MAX, u32::MAX),
            usize::MAX.checked_mul(4)
        );
        assert!(has_exact_bgra8_len(2, 3, 24));
        assert!(!has_exact_bgra8_len(2, 3, 23));
        assert!(!has_exact_bgra8_len(0, 3, 0));
        assert!(!has_exact_bgra8_len(i32::MAX as u32 + 1, 1, 4));
    }
}
