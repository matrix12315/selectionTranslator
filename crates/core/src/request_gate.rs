//! Admission control for remote translation requests.
//!
//! All checks that can prevent a provider call happen here. In particular,
//! [`PreparedRequest`] has private fields and no public constructor.

use std::collections::{HashMap, HashSet};

use crate::job::JobInput;
use crate::normalize::{normalize_optional, normalize_text, sanitize_hover_text_context};
pub use crate::prompt::{PromptConfig, RenderedPrompt};
use crate::sentence::sentence_for_target;

/// The maximum target size, measured in Unicode scalar values (not bytes).
pub const MAX_TARGET_SCALARS: usize = 4_000;

/// The response format expected by the popup renderer. This is appended to
/// every admitted profile after its placeholders have been rendered, so a
/// profile can still customize the task while the provider gets a stable,
/// renderer-friendly output contract.
const MARKDOWN_RESPONSE_CONTRACT: &str = "Return the response as valid, concise Markdown. Plain text is valid Markdown. Use headings, lists, emphasis, and code formatting only when useful. Do not wrap the entire response in a fenced code block.";

/// Provider settings needed before a request can be admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConfig {
    pub endpoint: String,
    pub model: String,
}

impl ProviderConfig {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }

    pub fn is_valid(&self) -> bool {
        let endpoint = self.endpoint.trim();
        let model = normalize_text(&self.model);
        if endpoint.is_empty() || model.is_empty() || model != self.model {
            return false;
        }

        let Some((scheme, authority_and_path)) = endpoint.split_once("://") else {
            return false;
        };
        let scheme_is_valid = scheme.eq_ignore_ascii_case("https")
            || (scheme.eq_ignore_ascii_case("http") && is_loopback_authority(authority_and_path));
        let authority = authority_and_path
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        scheme_is_valid && !authority.is_empty()
    }
}

fn is_loopback_authority(value: &str) -> bool {
    let authority = value
        .rsplit_once('@')
        .map_or(value, |(_, host)| host)
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_matches(['[', ']']);
    matches!(authority, "localhost" | "127.0.0.1" | "::1")
}

/// Why an input was prevented from becoming a provider request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestRejection {
    MissingTarget,
    TargetTooLong { scalars: usize, maximum: usize },
    Cancelled,
    StaleJob { input_id: u64, active_id: u64 },
    MissingPrompt,
    InvalidPrompt,
    MissingProviderConfig,
    InvalidProviderConfig,
}

/// A validated request. Fields are intentionally private: this type can only
/// be instantiated by [`prepare_request`] in this module.
#[derive(Clone, Debug)]
pub struct PreparedRequest {
    job_id: u64,
    target: String,
    context: Option<String>,
    prompt_id: String,
    rendered_prompt: RenderedPrompt,
    model: String,
    temperature: Option<f32>,
    max_output_tokens: Option<u32>,
}

impl PartialEq for PreparedRequest {
    fn eq(&self, other: &Self) -> bool {
        self.job_id == other.job_id
            && self.target == other.target
            && self.context == other.context
            && self.prompt_id == other.prompt_id
            && self.rendered_prompt == other.rendered_prompt
            && self.model == other.model
            && self.temperature.map(f32::to_bits) == other.temperature.map(f32::to_bits)
            && self.max_output_tokens == other.max_output_tokens
    }
}

impl Eq for PreparedRequest {}

impl PreparedRequest {
    /// Bind an already validated request to the nonzero job identifier issued
    /// by Coordinator. Platform code uses this after an identical same-thread
    /// preflight, avoiding a second fallible validation after job commit.
    pub fn bind_job_id(mut self, job_id: u64) -> Self {
        assert_ne!(job_id, 0, "coordinator job IDs are nonzero");
        self.job_id = job_id;
        self
    }

    pub fn job_id(&self) -> u64 {
        self.job_id
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    pub fn prompt_id(&self) -> &str {
        &self.prompt_id
    }

    /// The validated system message rendered from the selected profile.
    pub fn system_prompt(&self) -> &str {
        &self.rendered_prompt.system_prompt
    }

    /// The validated user message rendered from the selected profile.
    pub fn user_prompt(&self) -> &str {
        &self.rendered_prompt.user_prompt
    }

    /// The profile override, or the provider default when the profile does
    /// not specify a model.
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    pub fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
}

/// Stateful convenience wrapper around the pure gate function.
pub struct RequestGate {
    provider: Option<ProviderConfig>,
    prompts: HashMap<String, PromptConfig>,
    duplicate_prompt_ids: HashSet<String>,
}

impl RequestGate {
    pub fn new(provider: ProviderConfig, prompts: impl IntoIterator<Item = PromptConfig>) -> Self {
        Self::with_optional_provider(Some(provider), prompts)
    }

    pub fn without_provider(prompts: impl IntoIterator<Item = PromptConfig>) -> Self {
        Self::with_optional_provider(None, prompts)
    }

    pub fn with_optional_provider(
        provider: Option<ProviderConfig>,
        prompts: impl IntoIterator<Item = PromptConfig>,
    ) -> Self {
        let mut prompt_map = HashMap::new();
        let mut duplicate_prompt_ids = HashSet::new();
        for prompt in prompts {
            if prompt_map.contains_key(&prompt.id) {
                duplicate_prompt_ids.insert(prompt.id.clone());
            } else {
                prompt_map.insert(prompt.id.clone(), prompt);
            }
        }
        Self {
            provider,
            prompts: prompt_map,
            duplicate_prompt_ids,
        }
    }

    pub fn prepare(
        &self,
        input: &JobInput,
        active_job_id: u64,
        cancelled: bool,
    ) -> Result<PreparedRequest, RequestRejection> {
        if self.duplicate_prompt_ids.contains(&input.prompt_id) {
            return Err(RequestRejection::InvalidPrompt);
        }
        prepare_request(
            input,
            active_job_id,
            cancelled,
            &self.prompts.values().cloned().collect::<Vec<_>>(),
            self.provider.as_ref(),
        )
    }
}

/// Validates and normalizes an input before any provider call is possible.
pub fn prepare_request(
    input: &JobInput,
    active_job_id: u64,
    cancelled: bool,
    prompts: &[PromptConfig],
    provider: Option<&ProviderConfig>,
) -> Result<PreparedRequest, RequestRejection> {
    if cancelled {
        return Err(RequestRejection::Cancelled);
    }
    if input.id != active_job_id {
        return Err(RequestRejection::StaleJob {
            input_id: input.id,
            active_id: active_job_id,
        });
    }

    let hover_text = (input.trigger == crate::TriggerKind::Hover)
        .then(|| sanitize_hover_text_context(input.text.clone()))
        .flatten();
    if input.trigger == crate::TriggerKind::Hover && hover_text.is_none() {
        return Err(RequestRejection::MissingTarget);
    }
    let target = hover_text.as_ref().map_or_else(
        || normalize_text(&input.text.target),
        |text| text.target.clone(),
    );
    if target.is_empty() {
        return Err(RequestRejection::MissingTarget);
    }
    let scalars = target.chars().count();
    if scalars > MAX_TARGET_SCALARS {
        return Err(RequestRejection::TargetTooLong {
            scalars,
            maximum: MAX_TARGET_SCALARS,
        });
    }

    if input.prompt_id.is_empty() {
        return Err(RequestRejection::MissingPrompt);
    }
    let matching_prompts = prompts.iter().filter(|prompt| prompt.id == input.prompt_id);
    let mut matching = matching_prompts.peekable();
    let Some(prompt) = matching.next() else {
        return Err(RequestRejection::MissingPrompt);
    };
    if matching.peek().is_some() || !prompt.is_valid() {
        return Err(RequestRejection::InvalidPrompt);
    }

    let Some(provider) = provider else {
        return Err(RequestRejection::MissingProviderConfig);
    };
    if !provider.is_valid() {
        return Err(RequestRejection::InvalidProviderConfig);
    }

    // Rendering is intentionally the final admission step. This keeps the
    // prompt renderer from doing work for cancelled, stale, empty, invalid,
    // or unconfigured requests, and means a rendered prompt is always safe to
    // hand to a provider.
    let context = if input.trigger == crate::TriggerKind::Hover {
        hover_text.and_then(|text| text.context)
    } else {
        normalize_optional(input.text.context.as_deref())
            .and_then(|context| sentence_for_target(&context, &target))
    };
    if input.trigger == crate::TriggerKind::Hover && context.is_none() {
        return Err(RequestRejection::MissingTarget);
    }
    let mut rendered_prompt = prompt
        .render(
            &target,
            context.as_deref(),
            &format!("{:?}", input.text.source),
        )
        .map_err(|_| RequestRejection::InvalidPrompt)?;
    // Keep the user-authored prompt intact and add the output contract only
    // after successful rendering. Since RenderedPrompt is stored in
    // PreparedRequest, the contract is automatically included in cache
    // identity and prompt edits cannot reuse an incompatible result.
    if rendered_prompt.system_prompt.is_empty() {
        rendered_prompt
            .system_prompt
            .push_str(MARKDOWN_RESPONSE_CONTRACT);
    } else {
        rendered_prompt.system_prompt.push_str("\n\n");
        rendered_prompt
            .system_prompt
            .push_str(MARKDOWN_RESPONSE_CONTRACT);
    }
    let model = prompt
        .model
        .clone()
        .unwrap_or_else(|| provider.model.clone());

    Ok(PreparedRequest {
        job_id: input.id,
        target,
        context,
        prompt_id: prompt.id.clone(),
        rendered_prompt,
        model,
        temperature: prompt.temperature,
        max_output_tokens: prompt.max_output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExtractionSource, TextContext, TriggerKind};

    fn provider() -> ProviderConfig {
        ProviderConfig::new("https://example.invalid/v1", "test-model")
    }

    fn prompt() -> PromptConfig {
        PromptConfig::new("translate")
    }

    fn input(id: u64, target: &str, context: Option<&str>) -> JobInput {
        JobInput::new(
            id,
            TriggerKind::Manual,
            TextContext {
                target: target.to_owned(),
                context: context.map(str::to_owned),
                source: ExtractionSource::Clipboard,
                screen_rect: None,
            },
            "translate",
        )
    }

    fn hover_input(id: u64, target: &str, context: Option<&str>) -> JobInput {
        JobInput::new(
            id,
            TriggerKind::Hover,
            TextContext {
                target: target.to_owned(),
                context: context.map(str::to_owned),
                source: ExtractionSource::UiaPoint,
                screen_rect: None,
            },
            "translate",
        )
    }

    #[test]
    fn valid_input_is_normalized_and_prepared() {
        let prompts = [prompt()];
        let result = prepare_request(
            &input(
                7,
                "\u{2003}hello\u{200B}",
                Some("\u{3000}A hello context.\u{FEFF}"),
            ),
            7,
            false,
            &prompts,
            Some(&provider()),
        )
        .expect("valid request");
        assert_eq!(result.job_id(), 7);
        assert_eq!(result.target(), "hello");
        assert_eq!(result.context(), Some("A hello context."));
        assert_eq!(result.prompt_id(), "translate");
        assert!(result.system_prompt().contains(MARKDOWN_RESPONSE_CONTRACT));
        assert_eq!(result.user_prompt(), "hello");
        assert_eq!(result.model(), "test-model");
        assert_eq!(result.temperature(), None);
        assert_eq!(result.max_output_tokens(), None);
    }

    #[test]
    fn validated_preflight_can_be_bound_to_a_coordinator_job_id() {
        let prepared = prepare_request(
            &input(0, "hello", None),
            0,
            false,
            &[prompt()],
            Some(&provider()),
        )
        .expect("preflight request");
        let bound = prepared.bind_job_id(42);
        assert_eq!(bound.job_id(), 42);
        assert_eq!(bound.target(), "hello");
        assert_eq!(bound.prompt_id(), "translate");
    }

    #[test]
    fn hover_is_sanitized_again_at_the_provider_boundary() {
        let result = prepare_request(
            &hover_input(
                9,
                "• **hello,**",
                Some("Before. Read **hello,** here. After."),
            ),
            9,
            false,
            &[prompt()],
            Some(&provider()),
        )
        .expect("decorated Hover target remains a valid word");
        assert_eq!(result.target(), "hello");
        assert_eq!(result.context(), Some("Read **hello,** here."));
    }

    #[test]
    fn hover_without_a_word_and_containing_sentence_never_reaches_provider() {
        for job in [
            hover_input(1, "***", Some("Only punctuation *** here.")),
            hover_input(1, "😀", Some("Only an emoji 😀 here.")),
            hover_input(1, "foo***bar", Some("Ambiguous foo***bar here.")),
            hover_input(1, "word", None),
            hover_input(1, "word", Some("An unrelated sentence.")),
            hover_input(1, "Word", Some("Only lowercase word occurs here.")),
        ] {
            assert_eq!(
                prepare_request(&job, 1, false, &[prompt()], Some(&provider())),
                Err(RequestRejection::MissingTarget)
            );
        }
    }

    #[test]
    fn non_hover_modes_keep_intentional_punctuation_and_multiline_text() {
        let manual = input(3, "  foo***bar\n()  ", Some("foo***bar\n()"));
        let prepared = prepare_request(&manual, 3, false, &[prompt()], Some(&provider()))
            .expect("Manual input keeps its existing normalization contract");
        assert_eq!(prepared.target(), "foo***bar\n()");

        let selection = JobInput::new(
            4,
            TriggerKind::Selection,
            TextContext {
                target: "  [foo***bar]\n()  ".into(),
                context: Some("[foo***bar]\n()".into()),
                source: ExtractionSource::UiaSelection,
                screen_rect: None,
            },
            "translate",
        );
        let prepared = prepare_request(&selection, 4, false, &[prompt()], Some(&provider()))
            .expect("Selection input keeps its existing normalization contract");
        assert_eq!(prepared.target(), "[foo***bar]\n()");
    }

    #[test]
    fn rejects_all_invalid_inputs() {
        let prompts = [prompt()];
        let cases = [
            (
                input(1, "", Some("context")),
                RequestRejection::MissingTarget,
            ),
            (
                input(1, "\u{200B}\u{2060}\u{FEFF}", None),
                RequestRejection::MissingTarget,
            ),
            (
                input(1, " \u{3000}\n", Some("context")),
                RequestRejection::MissingTarget,
            ),
            (
                input(1, &"x".repeat(MAX_TARGET_SCALARS + 1), None),
                RequestRejection::TargetTooLong {
                    scalars: MAX_TARGET_SCALARS + 1,
                    maximum: MAX_TARGET_SCALARS,
                },
            ),
        ];
        for (job, expected) in cases {
            assert_eq!(
                prepare_request(&job, 1, false, &prompts, Some(&provider())),
                Err(expected)
            );
        }
        assert_eq!(
            prepare_request(
                &input(1, "hello", None),
                1,
                true,
                &prompts,
                Some(&provider())
            ),
            Err(RequestRejection::Cancelled)
        );
        assert!(matches!(
            prepare_request(
                &input(1, "hello", None),
                2,
                false,
                &prompts,
                Some(&provider())
            ),
            Err(RequestRejection::StaleJob { .. })
        ));
        assert_eq!(
            prepare_request(&input(1, "hello", None), 1, false, &prompts, None),
            Err(RequestRejection::MissingProviderConfig)
        );
    }

    #[test]
    fn context_without_target_is_never_admitted() {
        let context_only = input(
            1,
            "\u{200B}\u{2060}\u{FEFF} \u{3000}",
            Some("a full sentence"),
        );
        assert_eq!(
            prepare_request(&context_only, 1, false, &[prompt()], Some(&provider())),
            Err(RequestRejection::MissingTarget)
        );
    }

    #[test]
    fn invalid_and_missing_prompts_are_rejected() {
        let mut missing = input(1, "hello", None);
        missing.prompt_id = "missing".to_owned();
        let prompts = [prompt()];
        assert_eq!(
            prepare_request(&missing, 1, false, &prompts, Some(&provider())),
            Err(RequestRejection::MissingPrompt)
        );

        let invalid = [PromptConfig::with_template("translate", "", "{unknown}")];
        assert_eq!(
            prepare_request(
                &input(1, "hello", None),
                1,
                false,
                &invalid,
                Some(&provider())
            ),
            Err(RequestRejection::InvalidPrompt)
        );
        assert_eq!(
            prepare_request(
                &input(1, "hello", None),
                1,
                false,
                &[prompt()],
                Some(&ProviderConfig::new("", "")),
            ),
            Err(RequestRejection::InvalidProviderConfig)
        );
    }

    #[test]
    fn profile_overrides_and_rendering_are_resolved_inside_the_gate() {
        let mut profile = PromptConfig::with_template(
            "translate",
            "You are a glossary assistant.",
            "Explain {target} using {context} from {source}.",
        );
        profile.model = Some("profile-model".to_owned());
        profile.temperature = Some(0.4);
        profile.max_output_tokens = Some(123);
        let result = prepare_request(
            &input(7, "term", Some("A sentence containing term.")),
            7,
            false,
            &[profile],
            Some(&provider()),
        )
        .expect("valid request");
        assert!(result
            .system_prompt()
            .starts_with("You are a glossary assistant."));
        assert!(result.system_prompt().contains(MARKDOWN_RESPONSE_CONTRACT));
        assert_eq!(
            result.user_prompt(),
            "Explain term using A sentence containing term. from Clipboard."
        );
        assert_eq!(result.model(), "profile-model");
        assert_eq!(result.temperature(), Some(0.4));
        assert_eq!(result.max_output_tokens(), Some(123));
    }

    #[test]
    fn context_is_reduced_to_the_one_sentence_containing_the_target() {
        let profile = PromptConfig::with_template(
            "translate",
            "Translate.",
            "Target={target}; Context={context}",
        );
        let result = prepare_request(
            &input(
                7,
                "term",
                Some("First sentence. Use the term here! Last sentence."),
            ),
            7,
            false,
            std::slice::from_ref(&profile),
            Some(&provider()),
        )
        .expect("valid request");
        assert_eq!(result.context(), Some("Use the term here!"));
        assert_eq!(
            result.user_prompt(),
            "Target=term; Context=Use the term here!"
        );

        let unrelated = prepare_request(
            &input(8, "term", Some("An unrelated sentence.")),
            8,
            false,
            &[profile],
            Some(&provider()),
        )
        .expect("valid request");
        assert_eq!(unrelated.context(), None);
        assert_eq!(unrelated.user_prompt(), "Target=term; Context=");
    }

    #[test]
    fn rendering_does_not_run_before_target_admission_checks() {
        let invalid_profile = PromptConfig::with_template("translate", "{unknown}", "{target}");
        let empty = input(1, "", None);
        assert_eq!(
            prepare_request(&empty, 1, false, &[invalid_profile], Some(&provider())),
            Err(RequestRejection::MissingTarget)
        );
    }

    #[test]
    fn markdown_contract_is_appended_after_custom_system_prompt() {
        let profile = PromptConfig::with_template(
            "translate",
            "Use the user's preferred terminology.",
            "Translate {target}.",
        );
        let result = prepare_request(
            &input(1, "hello", None),
            1,
            false,
            &[profile],
            Some(&provider()),
        )
        .expect("valid request");
        assert_eq!(
            result.system_prompt(),
            format!("Use the user's preferred terminology.\n\n{MARKDOWN_RESPONSE_CONTRACT}")
        );
        assert_eq!(result.user_prompt(), "Translate hello.");
    }
}
