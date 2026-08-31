//! Unicode normalization used at the request boundary.

use crate::{sentence::sentence_for_target_at, TextContext};

/// Formatting characters that are invisible but are not classified as
/// Unicode whitespace by Rust's `char::is_whitespace`.
pub const ZERO_WIDTH_FORMATTING: [char; 3] = ['\u{200B}', '\u{2060}', '\u{FEFF}'];

/// Removes the three disallowed zero-width formatting characters and trims
/// Unicode whitespace at both ends. Other characters, including interior
/// whitespace, are preserved exactly.
pub fn normalize_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !ZERO_WIDTH_FORMATTING.contains(character))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Named alias for callers validating a target at the request boundary.
pub fn normalize_target(value: &str) -> String {
    normalize_text(value)
}

/// Returns `None` for absent or formatting-only optional context.
pub fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(normalize_text)
        .filter(|normalized| !normalized.is_empty())
}

/// Reduces a coordinate-derived Hover target to one exact, contiguous token.
///
/// UI Automation and OCR occasionally include bullets, Markdown decoration,
/// quotes, or sentence punctuation in a word range. Boundary decoration is
/// harmless to remove, but deleting arbitrary characters from the middle
/// would invent text that was never present on screen. Ambiguous interior
/// separators are therefore rejected and may fall through to another local
/// extractor instead of reaching a provider.
pub fn sanitize_hover_target(value: &str) -> Option<String> {
    let normalized = normalize_text(value);
    if normalized.is_empty() || normalized.chars().any(char::is_control) {
        return None;
    }

    let start = normalized
        .char_indices()
        .find_map(|(index, character)| character.is_alphanumeric().then_some(index))?;
    let mut end = consume_word_segment(&normalized, start)?;

    loop {
        let remainder = &normalized[end..];
        let Some(connector_len) = connector_prefix_len(remainder) else {
            break;
        };
        let next = end + connector_len;
        if !normalized[next..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
        {
            break;
        }
        end = consume_word_segment(&normalized, next)?;
    }

    // Postfix increment and the C#/F# language names are meaningful code
    // tokens rather than decoration. Keep these two bounded suffix forms;
    // all other trailing punctuation is removed.
    let token = &normalized[start..end];
    let remainder = &normalized[end..];
    if let Some(after_increment) = remainder.strip_prefix("++") {
        if !after_increment.chars().any(char::is_alphanumeric) {
            end += 2;
        }
    } else if matches!(token, "C" | "c" | "F" | "f") {
        if let Some(after_hash) = remainder.strip_prefix('#') {
            if !after_hash.chars().any(char::is_alphanumeric) {
                end += 1;
            }
        }
    }

    let trailing = &normalized[end..];
    // Another lexical component after an unsupported separator (space,
    // emoji, arbitrary symbols, etc.) is ambiguous because a range alone
    // does not identify which component was under the pointer.
    if trailing.chars().any(char::is_alphanumeric) {
        return None;
    }

    Some(normalized[start..end].to_owned())
}

/// Applies Hover target cleanup while preserving the extractor's sentence,
/// source and geometry. A supplied context is retained only when it contains
/// the cleaned target; absent context remains eligible for bounded local OCR
/// enrichment in the platform pipeline.
pub fn sanitize_hover_text_context(mut text: TextContext) -> Option<TextContext> {
    text.target = sanitize_hover_target(&text.target)?;
    text.context = normalize_optional(text.context.as_deref()).and_then(|context| {
        let target_offset = context.find(&text.target)?;
        sentence_for_target_at(&context, &text.target, target_offset)
    });
    Some(text)
}

fn consume_word_segment(value: &str, start: usize) -> Option<usize> {
    let first = value[start..].chars().next()?;
    if !first.is_alphanumeric() {
        return None;
    }
    let rest_start = start + first.len_utf8();
    let mut end = rest_start;
    for (offset, character) in value[rest_start..].char_indices() {
        if character.is_alphanumeric() || is_combining_mark(character) {
            end = rest_start + offset + character.len_utf8();
            continue;
        }
        break;
    }
    Some(end)
}

fn is_combining_mark(character: char) -> bool {
    character != '_' && !character.is_alphanumeric() && unicode_ident::is_xid_continue(character)
}

fn connector_prefix_len(value: &str) -> Option<usize> {
    // Longest forms come first so `::` and arrows remain atomic.
    const CONNECTORS: &[&str] = &[
        "::", "->", "=>", "<-", "'", "’", "-", "‐", "‑", "‒", "–", "—", "_", ".", "/", "\\",
    ];
    CONNECTORS
        .iter()
        .find_map(|connector| value.starts_with(connector).then_some(connector.len()))
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_optional, normalize_text, sanitize_hover_target, sanitize_hover_text_context,
    };
    use crate::{ExtractionSource, TextContext};

    #[test]
    fn trims_unicode_whitespace_and_removes_zero_width_formatting() {
        assert_eq!(
            normalize_text("\u{2003}\u{200B} hello\u{2060} \u{FEFF}\n"),
            "hello"
        );
    }

    #[test]
    fn optional_context_drops_empty_values() {
        assert_eq!(normalize_optional(Some("\u{200B}\u{3000}")), None);
        assert_eq!(
            normalize_optional(Some(" context ")),
            Some("context".to_owned())
        );
    }

    #[test]
    fn hover_strips_boundary_decoration_without_touching_sentence_context() {
        let text = TextContext {
            target: "  • **“hello,”**  ".into(),
            context: Some("Before. The value is **“hello,”** here. After.".into()),
            source: ExtractionSource::UiaPoint,
            screen_rect: None,
        };
        let sanitized = sanitize_hover_text_context(text).expect("valid decorated word");
        assert_eq!(sanitized.target, "hello");
        assert_eq!(
            sanitized.context.as_deref(),
            Some("The value is **“hello,”** here.")
        );
    }

    #[test]
    fn hover_preserves_unicode_words_marks_and_supported_connectors() {
        for value in [
            "中文",
            "１２３",
            "e\u{301}",
            "don't",
            "isn’t",
            "state-of-the-art",
            "foo_bar",
            "object.member",
            "std::vector",
            "foo->bar",
            "key=>value",
            "path/to",
            "C++",
            "C#",
        ] {
            assert_eq!(sanitize_hover_target(value).as_deref(), Some(value));
        }
    }

    #[test]
    fn hover_rejects_nonwords_controls_and_ambiguous_interior_junk() {
        for value in [
            "***",
            "😀",
            "\u{301}",
            "foo\nbar",
            "foo bar",
            "foo😀bar",
            "foo***bar",
        ] {
            assert_eq!(sanitize_hover_target(value), None, "{value:?}");
        }
        assert_eq!(sanitize_hover_target("😀word😀").as_deref(), Some("word"));
        assert_eq!(
            sanitize_hover_target("【**“中文，”**】").as_deref(),
            Some("中文")
        );
    }

    #[test]
    fn hover_sanitization_is_idempotent() {
        for value in [
            "  • **“hello,”**  ",
            "【中文。】",
            "😀state-of-the-art😀",
            "`std::vector`",
            "C++",
        ] {
            let once = sanitize_hover_target(value).expect("test target should remain valid");
            assert_eq!(sanitize_hover_target(&once).as_deref(), Some(once.as_str()));
        }
    }

    #[test]
    fn hover_drops_context_that_does_not_contain_the_cleaned_target() {
        let text = TextContext {
            target: "**word**".into(),
            context: Some("A different sentence.".into()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };
        let sanitized = sanitize_hover_text_context(text).expect("target remains valid");
        assert_eq!(sanitized.target, "word");
        assert_eq!(sanitized.context, None);
    }

    #[test]
    fn hover_context_requires_an_exact_case_sensitive_target_occurrence() {
        let text = TextContext {
            target: "Word".into(),
            context: Some("Only lowercase word occurs here.".into()),
            source: ExtractionSource::UiaPoint,
            screen_rect: None,
        };
        let sanitized = sanitize_hover_text_context(text).expect("target remains valid");
        assert_eq!(sanitized.target, "Word");
        assert_eq!(sanitized.context, None);
    }

    #[test]
    fn hover_context_is_normalized_before_exact_containment_is_checked() {
        let text = TextContext {
            target: "\u{200b}word\u{2060}".into(),
            context: Some("Before. The wo\u{200b}rd remains exact after cleanup. After.".into()),
            source: ExtractionSource::Ocr,
            screen_rect: None,
        };
        let sanitized = sanitize_hover_text_context(text).expect("target remains valid");
        assert_eq!(sanitized.target, "word");
        assert_eq!(
            sanitized.context.as_deref(),
            Some("The word remains exact after cleanup.")
        );
    }
}
