//! Portable TOML configuration, defaults, and atomic persistence.

use std::collections::HashSet;
use std::fmt;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::prompt::PromptConfig;
use crate::text::TriggerKind;

pub const DEFAULT_CYCLE_PROFILES_HOTKEY: &str = "Ctrl+Alt+P";
pub const DEFAULT_CREDENTIAL_TARGET: &str = "SelectionTranslate/OpenAI";

/// Configuration stored at `%LOCALAPPDATA%\\SelectionTranslate\\config.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default = "default_profiles")]
    pub profiles: Vec<PromptConfig>,
    #[serde(default)]
    pub defaults: DefaultProfiles,
    #[serde(default)]
    pub provider: ProviderSettings,
    #[serde(default)]
    pub hotkeys: HotkeySettings,
    /// Presentation preferences. This section is optional for compatibility
    /// with configurations written before manager localization was added.
    #[serde(default)]
    pub ui: UiSettings,
}

/// Short alias for callers that prefer the generic configuration name.
pub type Config = AppConfig;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum UiLanguage {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiSettings {
    #[serde(default)]
    pub manager_language: UiLanguage,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultProfiles {
    #[serde(default = "default_selection_profile")]
    pub selection: String,
    #[serde(default = "default_hover_profile")]
    pub hover: String,
}

impl Default for DefaultProfiles {
    fn default() -> Self {
        Self {
            selection: default_selection_profile(),
            hover: default_hover_profile(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Name used by a platform credential store. This is never a secret.
    #[serde(default = "default_credential_target")]
    pub credential_target: String,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            model: default_model(),
            credential_target: default_credential_target(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeySettings {
    #[serde(default = "default_cycle_profiles_hotkey")]
    pub cycle_profiles: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            cycle_profiles: default_cycle_profiles_hotkey(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            profiles: default_profiles(),
            defaults: DefaultProfiles::default(),
            provider: ProviderSettings::default(),
            hotkeys: HotkeySettings::default(),
            ui: UiSettings::default(),
        }
    }
}

impl AppConfig {
    pub fn from_toml(contents: &str) -> Result<Self, ConfigError> {
        let document: toml::Value = toml::from_str(contents).map_err(ConfigError::Toml)?;
        ensure_complete_document(&document)?;
        let config: Self = toml::from_str(contents).map_err(ConfigError::Toml)?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        self.validate()?;
        toml::to_string_pretty(self).map_err(ConfigError::TomlSerialize)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::from_toml(&contents)
    }

    /// Validate the whole configuration before an adapter uses it.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.profiles.is_empty() {
            return Err(ConfigError::Validation(ValidationError::NoProfiles));
        }
        let mut ids = HashSet::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            if !ids.insert(profile.id.clone()) {
                return Err(ConfigError::Validation(
                    ValidationError::DuplicateProfileId(profile.id.clone()),
                ));
            }
            if let Some(error) = profile.validation_error() {
                return Err(ConfigError::Validation(ValidationError::InvalidProfile {
                    id: profile.id.clone(),
                    reason: error,
                }));
            }
        }
        if !ids.contains(&self.defaults.selection) {
            return Err(ConfigError::Validation(
                ValidationError::MissingDefaultProfile {
                    trigger: TriggerKind::Selection,
                    id: self.defaults.selection.clone(),
                },
            ));
        }
        if !ids.contains(&self.defaults.hover) {
            return Err(ConfigError::Validation(
                ValidationError::MissingDefaultProfile {
                    trigger: TriggerKind::Hover,
                    id: self.defaults.hover.clone(),
                },
            ));
        }
        if self.provider.endpoint.trim().is_empty()
            || self.provider.model.trim().is_empty()
            || self.provider.credential_target.trim().is_empty()
        {
            return Err(ConfigError::Validation(ValidationError::InvalidProvider));
        }
        if self.hotkeys.cycle_profiles.trim().is_empty() {
            return Err(ConfigError::Validation(ValidationError::InvalidHotkey));
        }
        Ok(())
    }

    pub fn default_profile_id(&self, trigger: TriggerKind) -> &str {
        match trigger {
            TriggerKind::Hover => &self.defaults.hover,
            TriggerKind::Selection | TriggerKind::Manual => &self.defaults.selection,
        }
    }

    pub fn profile(&self, id: &str) -> Option<&PromptConfig> {
        self.profiles.iter().find(|profile| profile.id == id)
    }
}

/// Return the platform's conventional configuration location.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|local_app_data| {
            PathBuf::from(local_app_data)
                .join("SelectionTranslate")
                .join("config.toml")
        })
}

pub fn parse_toml(contents: &str) -> Result<AppConfig, ConfigError> {
    AppConfig::from_toml(contents)
}

pub fn load_config(path: impl AsRef<Path>) -> Result<AppConfig, ConfigError> {
    AppConfig::load(path)
}

/// Serialize and replace a configuration file with a same-directory temp
/// file. The replacement is atomic on supported platforms.
pub fn save_atomic(path: impl AsRef<Path>, config: &AppConfig) -> Result<(), ConfigError> {
    let path = path.as_ref();
    let contents = config.to_toml()?;
    atomic_write(path, contents.as_bytes())
}

pub fn save_config_atomic(path: impl AsRef<Path>, config: &AppConfig) -> Result<(), ConfigError> {
    save_atomic(path, config)
}

/// Atomically replace a file with bytes written to a same-directory temp file.
pub fn atomic_write(path: impl AsRef<Path>, contents: &[u8]) -> Result<(), ConfigError> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(ConfigError::Io)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| ConfigError::InvalidPath(path.to_owned()))?;
    let temp_name = format!(
        "{}.tmp.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        unique_suffix()
    );
    let temp_path = parent.join(temp_name);
    let write_result = (|| -> Result<(), ConfigError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(ConfigError::Io)?;
        file.write_all(contents).map_err(ConfigError::Io)?;
        file.flush().map_err(ConfigError::Io)?;
        file.sync_all().map_err(ConfigError::Io)?;
        replace_file(&temp_path, path).map_err(ConfigError::Io)?;
        sync_directory(parent).map_err(ConfigError::Io)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // MoveFileExW performs the replacement in one filesystem operation.
        let result = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Toml(toml::de::Error),
    TomlSerialize(toml::ser::Error),
    InvalidPath(PathBuf),
    Validation(ValidationError),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "configuration I/O error: {error}"),
            Self::Toml(error) => write!(formatter, "invalid configuration TOML: {error}"),
            Self::TomlSerialize(error) => {
                write!(formatter, "cannot serialize configuration: {error}")
            }
            Self::InvalidPath(path) => write!(
                formatter,
                "configuration path has no file name: {}",
                path.display()
            ),
            Self::Validation(error) => write!(formatter, "invalid configuration: {error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, PartialEq)]
pub enum ValidationError {
    IncompleteConfig,
    NoProfiles,
    DuplicateProfileId(String),
    InvalidProfile {
        id: String,
        reason: crate::prompt::PromptValidationError,
    },
    MissingDefaultProfile {
        trigger: TriggerKind,
        id: String,
    },
    InvalidProvider,
    InvalidHotkey,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteConfig => formatter.write_str(
                "configuration must contain complete profiles, defaults, provider, and hotkeys sections",
            ),
            Self::NoProfiles => formatter.write_str("at least one prompt profile is required"),
            Self::DuplicateProfileId(id) => write!(formatter, "duplicate prompt profile id {id:?}"),
            Self::InvalidProfile { id, reason } => write!(formatter, "profile {id:?}: {reason:?}"),
            Self::MissingDefaultProfile { trigger, id } => write!(
                formatter,
                "{trigger:?} default profile {id:?} does not exist"
            ),
            Self::InvalidProvider => {
                formatter.write_str("provider endpoint, model, and credential target are required")
            }
            Self::InvalidHotkey => formatter.write_str("cycle-profiles hotkey is required"),
        }
    }
}

/// `serde(default)` keeps programmatic compatibility for callers constructing
/// values directly, but a file on disk must be a complete manager-produced
/// document. This prevents an empty or partially written config from silently
/// turning into a valid set of defaults during reload.
fn ensure_complete_document(document: &toml::Value) -> Result<(), ConfigError> {
    let Some(table) = document.as_table() else {
        return Err(ConfigError::Validation(ValidationError::IncompleteConfig));
    };
    for section in ["profiles", "defaults", "provider", "hotkeys"] {
        if !table.contains_key(section) {
            return Err(ConfigError::Validation(ValidationError::IncompleteConfig));
        }
    }

    let Some(profiles) = table.get("profiles").and_then(toml::Value::as_array) else {
        return Err(ConfigError::Validation(ValidationError::IncompleteConfig));
    };
    if profiles.is_empty()
        || profiles.iter().any(|profile| {
            let Some(profile) = profile.as_table() else {
                return true;
            };
            ["id", "name", "system_prompt", "user_template"]
                .iter()
                .any(|field| !profile.contains_key(*field))
        })
    {
        return Err(ConfigError::Validation(ValidationError::IncompleteConfig));
    }

    let required_fields = [
        ("defaults", ["selection", "hover"].as_slice()),
        (
            "provider",
            ["endpoint", "model", "credential_target"].as_slice(),
        ),
        ("hotkeys", ["cycle_profiles"].as_slice()),
    ];
    if required_fields.iter().any(|(section, fields)| {
        table
            .get(*section)
            .and_then(toml::Value::as_table)
            .is_none_or(|section| fields.iter().any(|field| !section.contains_key(*field)))
    }) {
        return Err(ConfigError::Validation(ValidationError::IncompleteConfig));
    }
    Ok(())
}

fn default_profiles() -> Vec<PromptConfig> {
    vec![
        PromptConfig {
            id: default_selection_profile(),
            name: "Contextual Chinese-English translation".to_owned(),
            system_prompt: "Translate Chinese and English naturally, preserving meaning and context. Return only the translation.".to_owned(),
            user_template: "Translate only Target. Use Context only to disambiguate:\nTarget:\n{target}\nContext:\n{context}".to_owned(),
            model: None,
            temperature: Some(0.2),
            max_output_tokens: Some(512),
        },
        PromptConfig {
            id: default_hover_profile(),
            name: "Word explanation".to_owned(),
            system_prompt: "Explain the word for a language learner with a concise definition, pronunciation, part of speech, and one example.".to_owned(),
            user_template: "Explain only Word. Use Sentence context only to disambiguate:\nWord:\n{target}\nSentence context:\n{context}".to_owned(),
            model: None,
            temperature: Some(0.2),
            max_output_tokens: Some(512),
        },
        PromptConfig {
            id: "wiki-style".to_owned(),
            name: "Wiki-style explanation".to_owned(),
            system_prompt: "Explain the target in a factual, neutral, wiki-style summary. State uncertainty when context is insufficient.".to_owned(),
            user_template: "Explain only Subject. Use Context only as supporting information:\nSubject:\n{target}\nContext:\n{context}".to_owned(),
            model: None,
            temperature: Some(0.2),
            max_output_tokens: Some(768),
        },
        PromptConfig {
            id: "linguist-analysis".to_owned(),
            name: "Expert linguist analysis".to_owned(),
            system_prompt: "You are an expert linguist and translator. Analyze the input and respond mainly in English. Return concise, valid Markdown using exactly these four level-2 headings, exactly once each, in this order, with no introduction, conclusion, alternate format, or repeated heading:\n## Translation\n- Chinese: Provide the Chinese translation.\n- English: If the input is not English, provide its natural English translation; otherwise write None.\n- Words: List key words as entries containing word, IPA/soundmark, part of speech (n./v./adj./etc.), and Chinese meaning.\n- Error check: First check spelling and grammar. If an error exists, identify the most likely intended wording and translate it; otherwise write None.\n## Idioms and Grammar\nBriefly explain usage, simple grammar points, and relevant common phrasal verbs or idioms; write None when not applicable.\n## Other Forms\nList common noun, verb, adjective, and adverb forms when applicable; write None when not applicable.\n## Reasoning\nFor non-English input, briefly explain how the English translation is natural and authentic rather than literal or Chinglish. For English input, briefly explain the nuance behind the Chinese translation choice. Write None when not applicable. Do not create any other heading, do not repeat a heading, and never use a `Translation:` field or add alternate translations. Use None for every inapplicable field.".to_owned(),
            user_template: "Analyze only this input:\n{target}\n\nSentence context for disambiguation only:\n{context}".to_owned(),
            model: None,
            temperature: Some(0.2),
            max_output_tokens: Some(700),
        },
        PromptConfig {
            id: "code-specialist".to_owned(),
            name: "Program specialist".to_owned(),
            system_prompt: "You are a program specialist. Examine the code first and briefly identify flaws. If it is correct, explain it according to the requested structure. Respond mainly in English using concise Markdown:\n1. Break down each part of the code.\n2. Explain what each line does.\n3. State the overall purpose of the code.".to_owned(),
            user_template: "Analyze this code snippet:\n{target}".to_owned(),
            model: None,
            temperature: Some(0.2),
            max_output_tokens: Some(700),
        },
        PromptConfig {
            id: "concise-explanation".to_owned(),
            name: "Concise explanation and context".to_owned(),
            system_prompt: "Explain the input content mainly in English. Start with a concise, non-redundant summary explanation. Then add a short section containing useful common knowledge related to the content.".to_owned(),
            user_template: "Input:\n{target}\n\nSentence context for disambiguation only:\n{context}".to_owned(),
            model: None,
            temperature: Some(0.2),
            max_output_tokens: Some(500),
        },
    ]
}

fn default_selection_profile() -> String {
    "contextual-zh-en".to_owned()
}
fn default_hover_profile() -> String {
    "word-explanation".to_owned()
}
fn default_endpoint() -> String {
    "https://api.openai.com/v1".to_owned()
}
fn default_model() -> String {
    "gpt-4o-mini".to_owned()
}
fn default_credential_target() -> String {
    DEFAULT_CREDENTIAL_TARGET.to_owned()
}
fn default_cycle_profiles_hotkey() -> String {
    DEFAULT_CYCLE_PROFILES_HOTKEY.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_editable_and_separate_by_trigger() {
        let config = AppConfig::default();
        config.validate().expect("valid defaults");
        assert_eq!(
            config.default_profile_id(TriggerKind::Selection),
            "contextual-zh-en"
        );
        assert_eq!(
            config.default_profile_id(TriggerKind::Hover),
            "word-explanation"
        );
        assert_eq!(config.hotkeys.cycle_profiles, DEFAULT_CYCLE_PROFILES_HOTKEY);
        assert!(config.profile("wiki-style").is_some());
    }

    #[test]
    fn linguist_prompt_has_one_ordered_output_schema_without_duplicate_translation_field() {
        let config = AppConfig::default();
        let prompt = config
            .profile("linguist-analysis")
            .expect("default linguist profile");
        let schema = prompt.system_prompt.as_str();
        let headings = [
            "## Translation",
            "## Idioms and Grammar",
            "## Other Forms",
            "## Reasoning",
        ];
        let positions: Vec<_> = headings
            .iter()
            .map(|heading| {
                assert_eq!(schema.matches(heading).count(), 1, "{heading:?}");
                schema.find(heading).expect("heading present")
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!schema.contains("- Translation:"));
        assert!(schema.contains("exactly these four level-2 headings"));
        assert!(schema.contains("never use a `Translation:` field"));
        assert!(schema.contains("Use None"));
    }

    #[test]
    fn toml_is_strict_and_rejects_duplicate_profiles() {
        let duplicate = r#"
            [[profiles]]
            id = "same"
            name = "One"
            system_prompt = "Translate"
            user_template = "{target}"
            [[profiles]]
            id = "same"
            name = "Two"
            system_prompt = "Translate"
            user_template = "{target}"
            [defaults]
            selection = "same"
            hover = "same"
            [provider]
            endpoint = "https://example.invalid/v1"
            model = "model"
            credential_target = "target"
            [hotkeys]
            cycle_profiles = "Ctrl+Alt+P"
        "#;
        assert!(matches!(
            AppConfig::from_toml(duplicate),
            Err(ConfigError::Validation(
                ValidationError::DuplicateProfileId(_)
            ))
        ));
        assert!(AppConfig::from_toml("api_key = 'never'").is_err());
    }

    #[test]
    fn empty_and_partial_files_are_invalid_but_complete_files_round_trip() {
        assert!(AppConfig::from_toml("").is_err());
        assert!(AppConfig::from_toml("[provider]\nendpoint = 'https://example.invalid'").is_err());
        let complete = AppConfig::default().to_toml().expect("serialize defaults");
        let loaded = AppConfig::from_toml(&complete).expect("complete serialized config");
        assert_eq!(loaded, AppConfig::default());
    }

    #[test]
    fn legacy_complete_config_without_ui_defaults_to_english() {
        let serialized = AppConfig::default().to_toml().expect("serialize defaults");
        let mut document: toml::Value = toml::from_str(&serialized).expect("parse serialized");
        document.as_table_mut().expect("root table").remove("ui");
        let legacy = toml::to_string_pretty(&document).expect("serialize legacy config");
        let loaded = AppConfig::from_toml(&legacy).expect("legacy config remains valid");
        assert_eq!(loaded.ui.manager_language, UiLanguage::English);
    }

    #[test]
    fn manager_language_round_trips_and_rejects_unknown_codes() {
        let mut config = AppConfig::default();
        config.ui.manager_language = UiLanguage::SimplifiedChinese;
        let serialized = config.to_toml().expect("serialize localized config");
        assert!(serialized.contains("manager_language = \"zh-CN\""));
        assert_eq!(
            AppConfig::from_toml(&serialized)
                .expect("localized config")
                .ui
                .manager_language,
            UiLanguage::SimplifiedChinese
        );
        let invalid = serialized.replace("zh-CN", "fr");
        assert!(AppConfig::from_toml(&invalid).is_err());
    }

    #[test]
    fn atomic_save_replaces_existing_file() {
        let directory =
            std::env::temp_dir().join(format!("selection-translate-config-{}", std::process::id()));
        let path = directory.join("config.toml");
        let config = AppConfig::default();
        save_atomic(&path, &config).expect("first atomic save");
        let loaded = AppConfig::load(&path).expect("load saved configuration");
        assert_eq!(loaded, config);
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(directory);
    }
}
