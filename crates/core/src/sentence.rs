//! Small, local sentence-boundary detector used by hover extraction.
//!
//! UI Automation exposes word, line, and paragraph units, but not sentences.
//! This module deliberately works on the bounded text returned by an adapter;
//! it never performs I/O and treats a paragraph without punctuation as one
//! sentence.

const ABBREVIATIONS: &[&str] = &[
    "mr.", "mrs.", "ms.", "dr.", "prof.", "sr.", "jr.", "st.", "vs.", "etc.", "e.g.", "i.e.",
    "fig.", "no.", "u.s.", "u.k.",
];

/// Return the sentence containing `target` in `context`.
pub fn sentence_for_target(context: &str, target: &str) -> Option<String> {
    let start = find_target(context, target)?;
    sentence_for_target_at(context, target, start)
}

/// Return the sentence containing a target at an explicitly supplied byte
/// offset. The offset must identify the requested occurrence in `context`;
/// this prevents repeated words from silently resolving to the first match.
///
/// The offset is a Rust UTF-8 byte offset. Adapters that receive UTF-16 text
/// should convert their range prefix to a Rust byte offset before calling it.
pub fn sentence_for_target_at(context: &str, target: &str, target_offset: usize) -> Option<String> {
    let needle = target.trim();
    if needle.is_empty() || !context.is_char_boundary(target_offset) {
        return None;
    }
    let candidate = context.get(target_offset..)?.get(..needle.len())?;
    if crate::normalize::normalize_target(candidate) != crate::normalize::normalize_target(needle) {
        return None;
    }
    let (left, right) = sentence_bounds(context, target_offset);
    let sentence = context.get(left..right)?.trim();
    (!sentence.is_empty()).then(|| sentence.to_owned())
}

/// Alias with a descriptive name for platform adapters.
pub fn extract_sentence_context(context: &str, target: &str) -> Option<String> {
    sentence_for_target(context, target)
}

/// Find the byte range of the sentence containing `offset`.
pub fn sentence_bounds(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let mut left = 0;
    for (index, ch) in text.char_indices() {
        if index >= offset {
            break;
        }
        if is_boundary(text, index, ch) {
            left = index + ch.len_utf8();
            while text[left..]
                .chars()
                .next()
                .is_some_and(|next| is_closing_quote(next) || next.is_whitespace())
            {
                left += text[left..].chars().next().map_or(0, char::len_utf8);
            }
        }
    }

    let mut right = text.len();
    for (index, ch) in text.char_indices().filter(|(index, _)| *index >= offset) {
        if is_boundary(text, index, ch) {
            right = index + ch.len_utf8();
            while right < text.len() {
                let next = text[right..].chars().next().expect("valid boundary");
                if !is_closing_quote(next) {
                    break;
                }
                right += next.len_utf8();
            }
            break;
        }
    }
    (left, right)
}

fn find_target(source: &str, target: &str) -> Option<usize> {
    let needle = target.trim();
    if needle.is_empty() {
        return None;
    }
    source.find(needle).or_else(|| {
        needle.is_ascii().then(|| {
            source.char_indices().find_map(|(index, _)| {
                source[index..]
                    .get(..needle.len())
                    .filter(|candidate| candidate.eq_ignore_ascii_case(needle))
                    .map(|_| index)
            })
        })?
    })
}

fn is_boundary(text: &str, index: usize, ch: char) -> bool {
    if !matches!(ch, '.' | '!' | '?' | '。' | '！' | '？' | '；' | ';') {
        return false;
    }
    if ch == '.' && is_abbreviation(text, index) {
        return false;
    }
    true
}

fn is_abbreviation(text: &str, index: usize) -> bool {
    let end = index + 1;
    let start = text[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace() || matches!(c, '(' | '[' | '"' | '\''))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let token = text[start..end].to_lowercase();
    if ABBREVIATIONS.contains(&token.as_str())
        || token.chars().all(|c| c.is_ascii_uppercase() || c == '.')
    {
        return true;
    }
    text[end..]
        .chars()
        .next()
        .is_some_and(|next| next.is_ascii_lowercase())
}

fn is_closing_quote(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | ')' | ']' | '}' | '”' | '’' | '»' | '』' | '」' | '》' | '）' | '］' | '｝'
    )
}

#[cfg(test)]
mod tests {
    use super::sentence_for_target_at;
    use super::{sentence_bounds, sentence_for_target};

    #[test]
    fn english_boundary_and_quotes() {
        assert_eq!(
            sentence_for_target("One starts. Two ends!", "Two"),
            Some("Two ends!".into())
        );
        assert_eq!(
            sentence_for_target("He said, \"Run fast!\" Then stop.", "Run"),
            Some("He said, \"Run fast!\"".into())
        );
    }

    #[test]
    fn abbreviations_do_not_split() {
        assert_eq!(
            sentence_for_target("Dr. Smith arrived. Next step.", "Smith"),
            Some("Dr. Smith arrived.".into())
        );
        assert_eq!(
            sentence_for_target("Use e.g. this value. Done.", "this"),
            Some("Use e.g. this value.".into())
        );
    }

    #[test]
    fn chinese_and_missing_punctuation() {
        assert_eq!(
            sentence_for_target("你好。世界！", "世界"),
            Some("世界！".into())
        );
        assert_eq!(
            sentence_for_target("没有句号的整段文字", "整段"),
            Some("没有句号的整段文字".into())
        );
    }

    #[test]
    fn bounds_are_safe_at_utf8_offsets() {
        let text = "甲。乙";
        assert_eq!(
            &text[sentence_bounds(text, 4).0..sentence_bounds(text, 4).1],
            "乙"
        );
    }

    #[test]
    fn missing_target_does_not_default_to_first_sentence() {
        assert_eq!(
            sentence_for_target("First sentence. Second sentence.", "absent"),
            None
        );
    }

    #[test]
    fn explicit_offset_selects_the_repeated_occurrence() {
        let text = "word first. word second.";
        let second = text.rfind("word").expect("second occurrence");
        assert_eq!(
            sentence_for_target_at(text, "word", second),
            Some("word second.".into())
        );
        assert_eq!(
            sentence_for_target_at(text, "word", text.find("word").unwrap()),
            Some("word first.".into())
        );
    }

    #[test]
    fn non_bmp_target_offsets_are_utf8_safe() {
        let text = "Prefix. 😀 target here.";
        let offset = text.find("😀").expect("emoji");
        assert_eq!(
            sentence_for_target_at(text, "😀", offset),
            Some("😀 target here.".into())
        );
        assert_eq!(sentence_for_target_at(text, "😀", offset + 1), None);
    }
}
