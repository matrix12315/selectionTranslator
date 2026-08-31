//! Small, deterministic post-processing for structured provider responses.

/// The built-in profile whose response contract requires these headings.
const LINGUIST_PROFILE_ID: &str = "linguist-analysis";
const HEADINGS: [&str; 4] = [
    "## Translation",
    "## Idioms and Grammar",
    "## Other Forms",
    "## Reasoning",
];

/// Normalize the terminal response for the built-in linguist profile.
///
/// Canonical heading lines and a small set of common model-emitted Markdown
/// variants are recognized; heading-like words in ordinary prose are left
/// untouched. Other profiles are returned byte-for-byte unchanged. The output
/// uses the input's newline convention and preserves all non-heading content.
pub fn normalize_terminal_response(profile_id: &str, response: &str) -> String {
    if profile_id != LINGUIST_PROFILE_ID {
        return response.to_owned();
    }

    let crlf = response.contains("\r\n");
    let separator = if crlf { "\r\n" } else { "\n" };
    let had_trailing_newline = response.ends_with('\n');
    let mut sections: [Vec<&str>; 4] = std::array::from_fn(|_| Vec::new());
    let mut preamble = Vec::new();
    let mut current: Option<usize> = None;
    let mut saw_heading = false;

    for raw in response.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let (heading, inline_content) = classify_heading(line);
        if let Some(index) = heading {
            saw_heading = true;
            current = Some(index);
            if let Some(content) = inline_content {
                sections[index].push(content);
            }
            continue;
        }
        match current {
            Some(index) => sections[index].push(line),
            None => preamble.push(line),
        }
    }

    // `split` yields one empty item for a terminal newline; it is represented
    // by the final newline below rather than as section content.
    if had_trailing_newline {
        if let Some(index) = current {
            if sections[index].last() == Some(&"") {
                sections[index].pop();
            }
        } else if preamble.last() == Some(&"") {
            preamble.pop();
        }
    }

    // The terminal schema always starts with Translation. Any unheaded
    // preamble is therefore retained as part of that section's body.
    if !saw_heading {
        sections[0] = preamble;
    } else if !preamble.is_empty() {
        let mut translation = preamble;
        translation.extend(sections[0].iter().copied());
        sections[0] = translation;
    }

    let mut lines: Vec<&str> = Vec::new();
    for (index, heading) in HEADINGS.iter().enumerate() {
        lines.push(heading);
        if sections[index].iter().all(|line| line.trim().is_empty()) {
            lines.push("None");
        } else {
            lines.extend(sections[index].iter().copied());
        }
    }
    let mut output = lines.join(separator);
    if had_trailing_newline {
        output.push_str(separator);
    }
    output
}

fn classify_heading(line: &str) -> (Option<usize>, Option<&str>) {
    let trimmed = line.trim();
    // Markdown ATX headings: one or more hashes, a required separating space,
    // and a canonical section name (optionally followed by a colon).
    if let Some(rest) = trimmed.strip_prefix('#') {
        let rest = rest.trim_start_matches('#');
        if rest.starts_with(char::is_whitespace) {
            let title = rest.trim().trim_end_matches(':').trim();
            if let Some(index) = canonical_name(title) {
                return (Some(index), None);
            }
        }
    }
    // Bold labels are occasionally emitted instead of Markdown headings.
    if let Some(inner) = trimmed
        .strip_prefix("**")
        .and_then(|s| s.strip_suffix("**"))
    {
        if let Some(index) = canonical_name(inner.trim().trim_end_matches(':').trim()) {
            return (Some(index), None);
        }
    }
    // A value-bearing field is not a heading, but its value belongs in the
    // corresponding section. Accept an optional list marker as well.
    let field = trimmed
        .strip_prefix('-')
        .map(str::trim_start)
        .unwrap_or(trimmed);
    for (index, heading) in HEADINGS.iter().enumerate() {
        let name = &heading[3..];
        if let Some(value) = field.strip_prefix(name).and_then(|s| s.strip_prefix(':')) {
            return (Some(index), Some(value.trim()));
        }
    }
    (None, None)
}

fn canonical_name(title: &str) -> Option<usize> {
    HEADINGS.iter().position(|heading| &heading[3..] == title)
}

#[cfg(test)]
mod tests {
    use super::normalize_terminal_response;

    #[test]
    fn removes_duplicate_headings_and_orders_sections() {
        let input = "## Reasoning\nr\n## Translation\nt\n## Translation\nagain\n## Other Forms\no";
        let output = normalize_terminal_response("linguist-analysis", input);
        assert_eq!(
            output,
            "## Translation\nt\nagain\n## Idioms and Grammar\nNone\n## Other Forms\no\n## Reasoning\nr"
        );
    }

    #[test]
    fn preserves_crlf_and_adds_missing_headings() {
        let input = "## Translation\r\nt\r\n## Reasoning\r\nr\r\n";
        let output = normalize_terminal_response("linguist-analysis", input);
        assert_eq!(output, "## Translation\r\nt\r\n## Idioms and Grammar\r\nNone\r\n## Other Forms\r\nNone\r\n## Reasoning\r\nr\r\n");
    }

    #[test]
    fn body_words_are_not_headings_and_other_profiles_are_unchanged() {
        let body = "Translation\n## Translation in prose\ntext";
        assert_eq!(normalize_terminal_response("linguist-analysis", body), "## Translation\nTranslation\n## Translation in prose\ntext\n## Idioms and Grammar\nNone\n## Other Forms\nNone\n## Reasoning\nNone");
        assert_eq!(normalize_terminal_response("wiki-style", body), body);
    }

    #[test]
    fn moves_preamble_into_translation_and_fills_empty_sections() {
        let output = normalize_terminal_response(
            "linguist-analysis",
            "intro\n## Reasoning\nr\n## Translation\n",
        );
        assert_eq!(output, "## Translation\nintro\n## Idioms and Grammar\nNone\n## Other Forms\nNone\n## Reasoning\nr\n");
    }

    #[test]
    fn canonicalizes_heading_variants_and_retains_field_values() {
        let input = "# Translation\n### Idioms and Grammar:\n**Other Forms**\nTranslation: 翻译\n- Reasoning: 思路";
        let output = normalize_terminal_response("linguist-analysis", input);
        assert_eq!(output, "## Translation\n翻译\n## Idioms and Grammar\nNone\n## Other Forms\nNone\n## Reasoning\n思路");
    }

    #[test]
    fn ordinary_prose_with_section_words_is_not_consumed() {
        let input = "This sentence mentions Translation: 翻译.\nA ## Translation in prose";
        let output = normalize_terminal_response("linguist-analysis", input);
        assert!(output.contains("This sentence mentions Translation: 翻译."));
        assert!(output.contains("A ## Translation in prose"));
    }
}
