//! Small in-memory result cache used after request admission.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use crate::PreparedRequest;

pub const DEFAULT_CAPACITY: usize = 256;
pub const DEFAULT_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub struct CacheKey {
    pub target: String,
    pub context: Option<String>,
    pub prompt_id: String,
    pub model: String,
    /// The profile's temperature. It is part of the identity even though
    /// `f32` needs bitwise equality for a stable hash key.
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    /// Rendered messages are part of the identity so editing a prompt while
    /// retaining its profile id can never return an old answer.
    pub rendered_system_prompt: String,
    pub rendered_user_prompt: String,
    /// Extraction and provider identity prevent results from crossing
    /// semantically different input paths or endpoints.
    pub extraction_source: String,
    pub provider_identity: String,
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.context == other.context
            && self.prompt_id == other.prompt_id
            && self.model == other.model
            && self.temperature.map(f32::to_bits) == other.temperature.map(f32::to_bits)
            && self.max_output_tokens == other.max_output_tokens
            && self.rendered_system_prompt == other.rendered_system_prompt
            && self.rendered_user_prompt == other.rendered_user_prompt
            && self.extraction_source == other.extraction_source
            && self.provider_identity == other.provider_identity
    }
}

impl Eq for CacheKey {}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.target.hash(state);
        self.context.hash(state);
        self.prompt_id.hash(state);
        self.model.hash(state);
        self.temperature.map(f32::to_bits).hash(state);
        self.max_output_tokens.hash(state);
        self.rendered_system_prompt.hash(state);
        self.rendered_user_prompt.hash(state);
        self.extraction_source.hash(state);
        self.provider_identity.hash(state);
    }
}

impl CacheKey {
    pub fn new(
        target: impl Into<String>,
        context: Option<String>,
        prompt_id: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            target: target.into(),
            context,
            prompt_id: prompt_id.into(),
            model: model.into(),
            temperature: None,
            max_output_tokens: None,
            rendered_system_prompt: String::new(),
            rendered_user_prompt: String::new(),
            extraction_source: String::new(),
            provider_identity: String::new(),
        }
    }

    pub fn with_inference(
        target: impl Into<String>,
        context: Option<String>,
        prompt_id: impl Into<String>,
        model: impl Into<String>,
        temperature: Option<f32>,
        max_output_tokens: Option<u32>,
    ) -> Self {
        Self {
            target: target.into(),
            context,
            prompt_id: prompt_id.into(),
            model: model.into(),
            temperature,
            max_output_tokens,
            rendered_system_prompt: String::new(),
            rendered_user_prompt: String::new(),
            extraction_source: String::new(),
            provider_identity: String::new(),
        }
    }

    /// Builds the cache identity from an admitted request. This is the
    /// preferred constructor because it cannot omit a request's inference
    /// parameters.
    pub fn from_prepared(request: &PreparedRequest) -> Self {
        let mut key = Self::with_inference(
            request.target(),
            request.context().map(str::to_owned),
            request.prompt_id(),
            request.model(),
            request.temperature(),
            request.max_output_tokens(),
        );
        key.rendered_system_prompt = request.system_prompt().to_owned();
        key.rendered_user_prompt = request.user_prompt().to_owned();
        key
    }

    /// Builds a complete production identity from an admitted request and
    /// the platform facts that are intentionally not part of that request's
    /// provider payload.
    pub fn from_prepared_with_identity(
        request: &PreparedRequest,
        extraction_source: impl Into<String>,
        provider_identity: impl Into<String>,
    ) -> Self {
        let mut key = Self::from_prepared(request);
        key.extraction_source = extraction_source.into();
        key.provider_identity = provider_identity.into();
        key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedResult {
    pub output: String,
    inserted_at: Instant,
}

/// A bounded insertion-ordered cache. Entries are evicted oldest-first and
/// never served after the configured TTL.
#[derive(Debug)]
pub struct ResultCache {
    capacity: usize,
    ttl: Duration,
    entries: VecDeque<(CacheKey, CachedResult)>,
}

impl Default for ResultCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_TTL)
    }
}

impl ResultCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            entries: VecDeque::new(),
        }
    }

    pub fn insert(&mut self, key: CacheKey, output: impl Into<String>) {
        if self.capacity == 0 {
            return;
        }
        self.entries.retain(|(existing, _)| existing != &key);
        self.entries.push_back((
            key,
            CachedResult {
                output: output.into(),
                inserted_at: Instant::now(),
            },
        ));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<&str> {
        self.remove_expired();
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, result)| result.output.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn remove_expired(&mut self) {
        let ttl = self.ttl;
        self.entries
            .retain(|(_, result)| result.inserted_at.elapsed() <= ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheKey, ResultCache};
    use std::time::Duration;

    fn key(temperature: Option<f32>, max_output_tokens: Option<u32>) -> CacheKey {
        CacheKey::with_inference(
            "target",
            Some("context".to_owned()),
            "prompt",
            "model",
            temperature,
            max_output_tokens,
        )
    }

    #[test]
    fn inference_parameters_are_part_of_cache_identity() {
        let mut cache = ResultCache::new(8, Duration::from_secs(60));
        cache.insert(key(Some(0.2), Some(100)), "low");
        cache.insert(key(Some(0.7), Some(100)), "high");
        assert_eq!(cache.get(&key(Some(0.2), Some(100))), Some("low"));
        assert_eq!(cache.get(&key(Some(0.7), Some(100))), Some("high"));
        assert_eq!(cache.get(&key(Some(0.2), Some(200))), None);
    }

    #[test]
    fn capacity_and_expiry_are_enforced() {
        let mut cache = ResultCache::new(1, Duration::from_millis(0));
        cache.insert(key(None, None), "expired");
        assert_eq!(cache.get(&key(None, None)), None);
    }

    #[test]
    fn rendered_prompts_source_and_provider_are_part_of_identity() {
        let mut base = key(None, None);
        base.rendered_system_prompt = "translate".into();
        base.rendered_user_prompt = "hello".into();
        base.extraction_source = "UiaSelection".into();
        base.provider_identity = "https://one.example|model".into();
        let mut cache = ResultCache::new(8, Duration::from_secs(60));
        cache.insert(base.clone(), "one");

        let mut changed = base.clone();
        changed.rendered_user_prompt = "hello with context".into();
        assert_eq!(cache.get(&changed), None);
        changed = base.clone();
        changed.extraction_source = "Ocr".into();
        assert_eq!(cache.get(&changed), None);
        changed = base;
        changed.provider_identity = "https://two.example|model".into();
        assert_eq!(cache.get(&changed), None);
    }
}
