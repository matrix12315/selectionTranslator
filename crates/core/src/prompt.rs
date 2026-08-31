//! Prompt profiles and safe, target-gated rendering.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::normalize::{normalize_optional, normalize_text};

/// A user-editable prompt profile.
///
/// A profile deliberately contains no credential material. Provider
/// credentials are looked up by the platform adapter using the credential
/// target name in [`crate::config::ProviderSettings`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub system_prompt: String,
    pub user_template: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

impl PromptConfig {
    /// Creates a minimal valid profile useful for adapters and tests.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            system_prompt: String::new(),
            user_template: "{target}".to_owned(),
            model: None,
            temperature: None,
            max_output_tokens: None,
        }
    }

    pub fn with_template(
        id: impl Into<String>,
        system_prompt: impl Into<String>,
        user_template: impl Into<String>,
    ) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            system_prompt: system_prompt.into(),
            user_template: user_template.into(),
            model: None,
            temperature: None,
            max_output_tokens: None,
        }
    }

    /// Returns whether this profile is safe to pass to a provider.
    pub fn is_valid(&self) -> bool {
        self.validation_error().is_none()
    }

    pub fn validate(&self) -> Result<(), PromptValidationError> {
        self.validation_error().map_or(Ok(()), Err)
    }

    pub(crate) fn validation_error(&self) -> Option<PromptValidationError> {
        if normalize_text(&self.id) != self.id {
            return Some(PromptValidationError::InvalidId);
        }
        if self.id.is_empty() {
            return Some(PromptValidationError::EmptyId);
        }
        if self.name.trim().is_empty() {
            return Some(PromptValidationError::EmptyName);
        }
        if let Err(error) = validate_template(&self.system_prompt, false) {
            return Some(error);
        }
        if let Err(error) = validate_template(&self.user_template, true) {
            return Some(error);
        }
        if self
            .model
            .as_deref()
            .is_some_and(|model| normalize_text(model) != model || model.is_empty())
        {
            return Some(PromptValidationError::InvalidModel);
        }
        if self.temperature.is_some_and(|temperature| {
            !temperature.is_finite() || !(0.0..=2.0).contains(&temperature)
        }) {
            return Some(PromptValidationError::InvalidTemperature);
        }
        if self.max_output_tokens.is_some_and(|tokens| tokens == 0) {
            return Some(PromptValidationError::InvalidMaxOutputTokens);
        }
        None
    }

    /// Render both prompt messages. Rendering cannot succeed for an absent,
    /// whitespace-only, or zero-width-only target.
    pub fn render(
        &self,
        target: &str,
        context: Option<&str>,
        source: &str,
    ) -> Result<RenderedPrompt, PromptRenderError> {
        if !self.is_valid() {
            return Err(PromptRenderError::InvalidProfile);
        }
        let target = normalize_text(target);
        if target.is_empty() {
            return Err(PromptRenderError::MissingTarget);
        }
        let context = normalize_optional(context);
        let values = PlaceholderValues {
            target: &target,
            context: context.as_deref().unwrap_or_default(),
            source,
        };
        Ok(RenderedPrompt {
            system_prompt: render_template(&self.system_prompt, &values)
                .expect("validated system prompt"),
            user_prompt: render_template(&self.user_template, &values)
                .expect("validated user template"),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptValidationError {
    EmptyId,
    InvalidId,
    EmptyName,
    EmptyTemplate,
    MissingTargetPlaceholder,
    UnknownPlaceholder(String),
    MalformedPlaceholder,
    InvalidModel,
    InvalidTemperature,
    InvalidMaxOutputTokens,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptRenderError {
    InvalidProfile,
    MissingTarget,
}

impl fmt::Display for PromptValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PromptValidationError {}

impl fmt::Display for PromptRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PromptRenderError {}

/// The rendered messages sent to a provider. This type can only be created
/// through [`PromptConfig::render`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedPrompt {
    pub system_prompt: String,
    pub user_prompt: String,
}

struct PlaceholderValues<'a> {
    target: &'a str,
    context: &'a str,
    source: &'a str,
}

fn validate_template(template: &str, require_target: bool) -> Result<(), PromptValidationError> {
    if template.is_empty() {
        return if require_target {
            Err(PromptValidationError::EmptyTemplate)
        } else {
            Ok(())
        };
    }
    let mut found_target = false;
    let mut chars = template.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        match character {
            '{' => {
                let Some((end, _)) = template[start + character.len_utf8()..]
                    .char_indices()
                    .find(|(_, character)| *character == '}')
                else {
                    return Err(PromptValidationError::MalformedPlaceholder);
                };
                let end = start + character.len_utf8() + end;
                let placeholder = &template[start + 1..end];
                if !matches!(placeholder, "target" | "context" | "source") {
                    return Err(PromptValidationError::UnknownPlaceholder(
                        placeholder.to_owned(),
                    ));
                }
                found_target |= placeholder == "target";
                // Consume the closing brace. The byte scan above is safe and
                // leaves the outer iterator at the next character.
                while chars.next_if(|(index, _)| *index <= end).is_some() {}
            }
            '}' => return Err(PromptValidationError::MalformedPlaceholder),
            _ => {}
        }
    }
    if require_target && !found_target {
        return Err(PromptValidationError::MissingTargetPlaceholder);
    }
    Ok(())
}

fn render_template(template: &str, values: &PlaceholderValues<'_>) -> Result<String, ()> {
    let mut rendered = String::with_capacity(template.len());
    let mut cursor = 0;
    while cursor < template.len() {
        let relative_start = template[cursor..].find('{');
        let Some(relative_start) = relative_start else {
            rendered.push_str(&template[cursor..]);
            break;
        };
        let start = cursor + relative_start;
        rendered.push_str(&template[cursor..start]);
        let relative_end = template[start..].find('}').ok_or(())?;
        let end = start + relative_end;
        let value = match &template[start + 1..end] {
            "target" => values.target,
            "context" => values.context,
            "source" => values.source,
            _ => return Err(()),
        };
        rendered.push_str(value);
        cursor = end + 1;
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_supported_placeholders_are_accepted() {
        let valid = PromptConfig::with_template("id", "Translate {source}", "{target} ({context})");
        assert!(valid.is_valid());
        for template in ["{unknown}", "{target", "target}", "{{target}}"] {
            assert!(
                !PromptConfig::with_template("id", "", template).is_valid(),
                "{template}"
            );
        }
    }

    #[test]
    fn rendering_requires_a_real_target() {
        let prompt = PromptConfig::with_template("id", "", "{target}|{context}|{source}");
        assert_eq!(
            prompt.render(" \u{200B}\u{FEFF}", None, "Selection"),
            Err(PromptRenderError::MissingTarget)
        );
        let rendered = prompt
            .render(" \u{2003}hello\u{200B}", Some(" context "), "Selection")
            .expect("rendered prompt");
        assert_eq!(rendered.user_prompt, "hello|context|Selection");
    }
}
