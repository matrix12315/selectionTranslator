#![cfg_attr(windows, windows_subsystem = "windows")]

mod history_writer;

use selection_core::{default_config_path, AppConfig, ProviderConfig, RequestGate};
use selection_platform_interface::{
    CancellationToken, HistoryStore, PopupSink, PreparedRequest,
    ProviderError as InterfaceProviderError, ProviderResult, TranslationProvider,
};
use selection_platform_windows::composition::{AppRuntime, ProviderRuntime};
use selection_platform_windows::credentials;
use selection_provider_openai::{
    CancellationToken as OpenAiCancellation, DeltaSink, OpenAiConfig, OpenAiProvider,
};
use selection_storage::{default_history_path, HistoryDatabase};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

struct OpenAiAdapter {
    provider: Arc<OpenAiProvider>,
}

struct DeltaChannel(mpsc::Sender<String>);

impl DeltaSink for DeltaChannel {
    fn on_delta(&mut self, delta: &str) {
        let _ = self.0.send(delta.to_owned());
    }
}

/// Convert the concrete provider's privacy-safe categories into the portable
/// platform contract.  Keep this mapping explicit: the platform layer and
/// popup must never receive provider response bodies, endpoints, request
/// content, credentials, or raw Windows error strings.
fn map_provider_error(error: selection_provider_openai::ProviderError) -> InterfaceProviderError {
    use selection_provider_openai::ProviderError as OpenAiError;

    match error {
        OpenAiError::InvalidConfiguration(_)
        | OpenAiError::UnsupportedScheme
        | OpenAiError::InvalidUrl
        | OpenAiError::InvalidRequest(_)
        | OpenAiError::InvalidHeader => InterfaceProviderError::Configuration,
        OpenAiError::Dns => InterfaceProviderError::Dns,
        OpenAiError::Tls => InterfaceProviderError::Tls,
        OpenAiError::Timeout => InterfaceProviderError::Timeout,
        OpenAiError::Transport => InterfaceProviderError::Transport,
        OpenAiError::HttpStatus(401 | 403) => InterfaceProviderError::Authentication,
        OpenAiError::HttpStatus(status) => InterfaceProviderError::HttpStatus(status),
        OpenAiError::RateLimited => InterfaceProviderError::RateLimited,
        OpenAiError::MalformedJson => InterfaceProviderError::MalformedResponse,
        OpenAiError::IncompleteResponse => InterfaceProviderError::IncompleteResponse,
        OpenAiError::ResponseTooLarge => InterfaceProviderError::ResponseTooLarge,
        OpenAiError::Cancelled => InterfaceProviderError::Cancelled,
        OpenAiError::UnsupportedPlatform => InterfaceProviderError::Unavailable,
    }
}

impl TranslationProvider for OpenAiAdapter {
    fn stream(
        &self,
        prepared: &PreparedRequest,
        cancellation: &CancellationToken,
        sink: &mut dyn PopupSink,
    ) -> ProviderResult {
        if cancellation.is_cancelled() {
            return Err(InterfaceProviderError::Cancelled);
        }
        let provider_token = OpenAiCancellation::new();
        let worker_token = provider_token.clone();
        let request = prepared.clone();
        let (delta_tx, delta_rx) = mpsc::channel();
        let provider = Arc::clone(&self.provider);
        let worker = thread::Builder::new()
            .name("selection-translate-openai-adapter".to_owned())
            .spawn(move || provider.stream(&request, worker_token, DeltaChannel(delta_tx)))
            .map_err(|_| InterfaceProviderError::Local("could not start provider worker".into()))?;

        loop {
            if cancellation.is_cancelled() {
                provider_token.cancel();
            }
            match delta_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(delta) => sink.update(prepared.job_id(), &delta),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if worker.is_finished() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let result = worker
            .join()
            .map_err(|_| InterfaceProviderError::Local("provider worker stopped".into()))?;
        if cancellation.is_cancelled() {
            return Err(InterfaceProviderError::Cancelled);
        }
        result.map_err(map_provider_error)
    }
}

fn main() {
    let runtime = build_runtime();
    if selection_platform_windows::app::run(runtime).is_err() {
        // The executable uses the Windows subsystem, so stderr is normally
        // invisible. Keep both channels generic and privacy-safe.
        eprintln!("Selection Translate resident could not start");
        selection_platform_windows::app::show_resident_startup_failure();
        std::process::exit(1);
    }
}

fn build_runtime() -> AppRuntime {
    let (config, startup_error) = load_config();
    let mut provider_runtime = build_provider(&config);
    if provider_runtime.error.is_none() {
        provider_runtime.error = startup_error;
    }
    let request_gate = RequestGate::with_optional_provider(
        provider_runtime.provider_config.clone(),
        config.profiles.clone(),
    );
    let history = build_history_store();
    AppRuntime {
        config,
        request_gate,
        provider: provider_runtime.provider,
        startup_error: provider_runtime.error,
        provider_reloader: Arc::new(build_provider),
        history,
    }
}

fn build_history_store() -> Option<Arc<dyn HistoryStore>> {
    let path = default_history_path()?;
    let database = match HistoryDatabase::open(path) {
        Ok(database) => database,
        Err(_) => {
            // History is optional. A storage failure must never disable
            // translation, and the path/target is intentionally not logged.
            eprintln!("Selection Translate history is unavailable");
            return None;
        }
    };
    Some(Arc::new(history_writer::HistoryWriter::start(database)))
}

fn build_provider(config: &AppConfig) -> ProviderRuntime {
    let endpoint = std::env::var("SELECTION_TRANSLATE_OPENAI_BASE_URL")
        .unwrap_or_else(|_| config.provider.endpoint.clone());
    let model = std::env::var("SELECTION_TRANSLATE_OPENAI_MODEL")
        .unwrap_or_else(|_| config.provider.model.clone());
    let environment_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("SELECTION_TRANSLATE_OPENAI_API_KEY"))
        .ok()
        .filter(|value| !value.trim().is_empty());

    let mut error = None;
    let api_key = if environment_key.is_some() {
        environment_key
    } else {
        match credentials::read_api_key(&config.provider.credential_target) {
            Ok(value) => value,
            Err(credential_error) => {
                if error.is_none() {
                    let _ = credential_error;
                    error = Some("Credential storage is unavailable".to_owned());
                }
                None
            }
        }
    };
    let provider_config = ProviderConfig::new(endpoint.clone(), model.clone());
    let provider = if error.is_none() && api_key.is_some() && provider_config.is_valid() {
        match OpenAiProvider::new(OpenAiConfig {
            base_url: endpoint,
            default_model: model,
            timeout: Duration::from_secs(30),
            api_key,
        }) {
            Ok(provider) => Some(Arc::new(OpenAiAdapter {
                provider: Arc::new(provider),
            }) as Arc<dyn TranslationProvider + Send + Sync>),
            Err(provider_error) => {
                let _ = provider_error;
                error = Some("Provider configuration is invalid".to_owned());
                None
            }
        }
    } else {
        if error.is_none() && api_key.is_none() {
            error = Some("OpenAI API key is not configured".to_owned());
        } else if error.is_none() && !provider_config.is_valid() {
            error = Some("provider configuration is invalid".to_owned());
        }
        None
    };

    let provider_config = if error.is_none() {
        Some(provider_config)
    } else {
        None
    };
    ProviderRuntime {
        provider_config,
        provider,
        error,
    }
}

fn load_config() -> (AppConfig, Option<String>) {
    let Some(path) = default_config_path() else {
        return (AppConfig::default(), None);
    };
    if !path.exists() {
        return (AppConfig::default(), None);
    }
    match AppConfig::load(&path) {
        Ok(config) => (config, None),
        Err(error) => {
            let _ = error;
            (
                AppConfig::default(),
                Some("Configuration file is invalid".to_owned()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::map_provider_error;
    use selection_platform_interface::ProviderError as InterfaceError;
    use selection_provider_openai::ProviderError as OpenAiError;

    #[test]
    fn maps_provider_categories_without_preserving_sensitive_details() {
        let cases = [
            (
                OpenAiError::InvalidConfiguration("api_key"),
                InterfaceError::Configuration,
            ),
            (OpenAiError::HttpStatus(401), InterfaceError::Authentication),
            (OpenAiError::HttpStatus(403), InterfaceError::Authentication),
            (
                OpenAiError::HttpStatus(503),
                InterfaceError::HttpStatus(503),
            ),
            (OpenAiError::RateLimited, InterfaceError::RateLimited),
            (OpenAiError::Dns, InterfaceError::Dns),
            (OpenAiError::Tls, InterfaceError::Tls),
            (OpenAiError::Timeout, InterfaceError::Timeout),
            (OpenAiError::Transport, InterfaceError::Transport),
            (
                OpenAiError::MalformedJson,
                InterfaceError::MalformedResponse,
            ),
            (
                OpenAiError::IncompleteResponse,
                InterfaceError::IncompleteResponse,
            ),
            (
                OpenAiError::ResponseTooLarge,
                InterfaceError::ResponseTooLarge,
            ),
            (OpenAiError::Cancelled, InterfaceError::Cancelled),
        ];
        for (source, expected) in cases {
            assert_eq!(map_provider_error(source), expected);
        }
    }

    #[test]
    fn unsupported_platform_is_not_reported_as_a_provider_response_error() {
        assert_eq!(
            map_provider_error(OpenAiError::UnsupportedPlatform),
            InterfaceError::Unavailable
        );
    }
}
