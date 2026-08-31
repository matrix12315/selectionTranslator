use crate::{error::ProviderResult, sse, ProviderError};
use selection_core::PreparedRequest;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
#[cfg(windows)]
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_READ_BYTES: u32 = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const OUTPUT_BYTES_PER_TOKEN: usize = 16;
const OUTPUT_BYTE_HEADROOM: usize = 1024;

/// A sink for text deltas. Implementations normally forward these to the
/// resident popup; the provider itself never retains or logs the text.
pub trait DeltaSink {
    fn on_delta(&mut self, delta: &str);
}

/// Cooperative cancellation shared by the coordinator and provider worker.
/// On Windows, cancelling also closes the currently active WinHTTP request,
/// which wakes a blocked receive/read immediately.
#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    active_request: Arc<AtomicUsize>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(windows)]
        {
            let handle = self.active_request.swap(0, Ordering::AcqRel);
            if handle != 0 {
                // WinHttpCloseHandle is documented to be callable while an
                // operation is pending and causes it to return promptly.
                unsafe {
                    let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(
                        handle as *mut core::ffi::c_void,
                    );
                }
            }
        }
    }

    fn timeout(&self) {
        self.timed_out.store(true, Ordering::Release);
        self.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn is_timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }

    #[cfg(windows)]
    fn set_active(&self, handle: *mut core::ffi::c_void) {
        self.active_request
            .store(handle as usize, Ordering::Release);
    }

    #[cfg(windows)]
    fn clear_active(&self, handle: *mut core::ffi::c_void) -> bool {
        self.active_request
            .compare_exchange(handle as usize, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// OpenAI-compatible chat-completions configuration.
pub struct OpenAiConfig {
    pub base_url: String,
    pub default_model: String,
    pub timeout: Duration,
    pub api_key: Option<String>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com".to_owned(),
            default_model: "gpt-4o-mini".to_owned(),
            timeout: Duration::from_secs(30),
            api_key: None,
        }
    }
}

impl OpenAiConfig {
    pub fn from_env() -> Result<Self, ProviderError> {
        let mut config = Self::default();
        if let Ok(value) = std::env::var("SELECTION_TRANSLATE_OPENAI_BASE_URL") {
            config.base_url = value;
        }
        if let Ok(value) = std::env::var("SELECTION_TRANSLATE_OPENAI_MODEL") {
            config.default_model = value;
        }
        config.api_key = std::env::var("OPENAI_API_KEY").ok();
        validate_url(&config.base_url)?;
        if config.default_model.trim().is_empty() {
            return Err(ProviderError::InvalidConfiguration("model"));
        }
        Ok(config)
    }
}

pub struct OpenAiProvider {
    config: Arc<OpenAiConfig>,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        validate_url(&config.base_url)?;
        if config.default_model.trim().is_empty() {
            return Err(ProviderError::InvalidConfiguration("model"));
        }
        if config.timeout.is_zero() {
            return Err(ProviderError::InvalidConfiguration("timeout"));
        }
        if let Some(api_key) = config.api_key.as_deref() {
            if api_key.trim().is_empty() {
                return Err(ProviderError::InvalidConfiguration("api_key"));
            }
            if contains_unsafe_header_character(api_key) {
                return Err(ProviderError::InvalidHeader);
            }
        }
        Ok(Self {
            config: Arc::new(config),
        })
    }

    pub fn from_env() -> Result<Self, ProviderError> {
        Self::new(OpenAiConfig::from_env()?)
    }

    /// Streams a prepared request on a dedicated worker thread. There is no
    /// async runtime in this crate, and no API accepts raw target text.
    pub fn stream(
        &self,
        request: &PreparedRequest,
        cancellation: CancellationToken,
        mut sink: impl DeltaSink + Send + 'static,
    ) -> ProviderResult {
        let payload = payload_from_request(request);
        let config = Arc::clone(&self.config);
        thread::Builder::new()
            .name("selection-provider-openai".to_owned())
            .spawn(move || stream_worker(&config, payload, cancellation, &mut sink))
            .map_err(|_| ProviderError::Transport)?
            .join()
            .map_err(|_| ProviderError::Transport)?
    }
}

struct RequestPayload {
    #[allow(dead_code)]
    job_id: u64,
    system_prompt: String,
    user_prompt: String,
    model: String,
    temperature: Option<f32>,
    max_output_tokens: Option<u32>,
}

fn payload_from_request(request: &PreparedRequest) -> RequestPayload {
    RequestPayload {
        job_id: request.job_id(),
        system_prompt: request.system_prompt().to_owned(),
        user_prompt: request.user_prompt().to_owned(),
        model: request.model().to_owned(),
        temperature: request.temperature(),
        max_output_tokens: request.max_output_tokens(),
    }
}

fn build_request_body(payload: &RequestPayload) -> ProviderResult<Vec<u8>> {
    let mut request = json!({
        "model": payload.model,
        "stream": true,
        "messages": [
            { "role": "system", "content": payload.system_prompt },
            { "role": "user", "content": payload.user_prompt },
        ],
    });
    let object = request
        .as_object_mut()
        .expect("chat completion payload is an object");
    // Qwen thinking-capable models can spend many seconds reasoning without
    // emitting a visible content delta. Selection translation prioritizes
    // time-to-first-token, so explicitly disable that provider extension.
    // Keep the non-standard field away from other OpenAI-compatible APIs.
    if is_qwen_model(&payload.model) {
        object.insert("enable_thinking".to_owned(), json!(false));
    }
    if let Some(temperature) = payload.temperature {
        object.insert("temperature".to_owned(), json!(temperature));
    }
    // `max_output_tokens` is the portable core/profile name. The wire name
    // is `max_tokens`, the broadly supported /v1/chat/completions parameter.
    if let Some(max_output_tokens) = payload.max_output_tokens {
        object.insert("max_tokens".to_owned(), json!(max_output_tokens));
    }
    serde_json::to_vec(&request).map_err(|_| ProviderError::MalformedJson)
}

fn is_qwen_model(model: &str) -> bool {
    model
        .trim_start()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("qwen"))
}

fn stream_worker(
    config: &OpenAiConfig,
    payload: RequestPayload,
    cancellation: CancellationToken,
    sink: &mut dyn DeltaSink,
) -> ProviderResult {
    if cancellation.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    let body = build_request_body(&payload)?;

    #[cfg(windows)]
    {
        let deadline = TimeoutGuard::start(config.timeout, cancellation.clone());
        let result = stream_winhttp(
            config,
            body,
            cancellation.clone(),
            sink,
            payload.max_output_tokens,
        );
        deadline.finish();
        if cancellation.is_timed_out() {
            Err(ProviderError::Timeout)
        } else {
            result
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (config, body, cancellation, sink);
        Err(ProviderError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
struct TimeoutGuard {
    state: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl TimeoutGuard {
    fn start(timeout: Duration, cancellation: CancellationToken) -> Self {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_state = Arc::clone(&state);
        let worker = thread::spawn(move || {
            let (lock, wake) = &*thread_state;
            let finished = lock.lock().expect("timeout guard lock");
            let (finished, timed_out) = wake
                .wait_timeout(finished, timeout)
                .expect("timeout guard wait");
            if !*finished && timed_out.timed_out() {
                cancellation.timeout();
            }
        });
        Self {
            state,
            worker: Some(worker),
        }
    }

    fn finish(mut self) {
        let (lock, wake) = &*self.state;
        *lock.lock().expect("timeout guard lock") = true;
        wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedUrl {
    secure: bool,
    host: String,
    port: u16,
    path: String,
}

fn validate_url(value: &str) -> Result<ParsedUrl, ProviderError> {
    let parsed = parse_url(value)?;
    if !parsed.secure && !is_loopback(&parsed.host) {
        return Err(ProviderError::UnsupportedScheme);
    }
    Ok(parsed)
}

fn parse_url(value: &str) -> Result<ParsedUrl, ProviderError> {
    let (scheme, rest) = value
        .split_once("://")
        .ok_or(ProviderError::UnsupportedScheme)?;
    let secure = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        false
    } else {
        return Err(ProviderError::UnsupportedScheme);
    };
    if rest.is_empty() || rest.contains('#') || rest.contains('@') {
        return Err(ProviderError::InvalidUrl);
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let suffix = &rest[authority_end..];
    if authority.is_empty() || authority.chars().any(char::is_whitespace) {
        return Err(ProviderError::InvalidUrl);
    }
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let close = stripped.find(']').ok_or(ProviderError::InvalidUrl)?;
        let host = stripped[..close].to_owned();
        let tail = &stripped[close + 1..];
        let port = if tail.is_empty() {
            if secure {
                443
            } else {
                80
            }
        } else {
            parse_port(tail.strip_prefix(':').ok_or(ProviderError::InvalidUrl)?)?
        };
        (host, port)
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .map_or((authority, None), |(host, port)| {
                if port.is_empty() || port.chars().all(|c| c.is_ascii_digit()) {
                    (host, Some(port))
                } else {
                    (authority, None)
                }
            });
        if host.contains(':') {
            return Err(ProviderError::InvalidUrl);
        }
        (
            host.to_owned(),
            port.map(parse_port)
                .transpose()?
                .unwrap_or(if secure { 443 } else { 80 }),
        )
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) || port == 0 {
        return Err(ProviderError::InvalidUrl);
    }
    if suffix.contains('?') {
        return Err(ProviderError::InvalidUrl);
    }
    let path = if suffix.is_empty() {
        "/".to_owned()
    } else {
        suffix.to_owned()
    };
    Ok(ParsedUrl {
        secure,
        host,
        port,
        path,
    })
}

fn parse_port(value: &str) -> Result<u16, ProviderError> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(ProviderError::InvalidUrl);
    }
    value.parse().map_err(|_| ProviderError::InvalidUrl)
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Converts an OpenAI-compatible base path into the chat-completions request
/// path. Providers commonly publish either a host-only base URL or a versioned
/// base URL such as `/v1` or `/compatible-mode/v1`; appending another `/v1`
/// to the latter produces an invalid endpoint.
fn chat_completions_path(base_path: &str) -> String {
    let base = base_path.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_owned()
    } else if base.is_empty() {
        "/v1/chat/completions".to_owned()
    } else if base
        .rsplit('/')
        .next()
        .is_some_and(|segment| segment.eq_ignore_ascii_case("v1"))
    {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

/// Select the WinHTTP access mode without consulting mutable process
/// environment variables. Remote HTTPS endpoints use Windows' configured
/// proxy (including WPAD/PAC); local test and helper endpoints stay direct.
///
/// Keeping this decision separate from the WinHTTP call makes the security
/// boundary and the loopback exception deterministic and testable. URL
/// validation still rejects non-loopback HTTP before this selector is used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProxyAccessMode {
    Direct,
    Automatic,
}

fn proxy_access_mode(parsed: &ParsedUrl) -> ProxyAccessMode {
    if is_loopback(&parsed.host) {
        ProxyAccessMode::Direct
    } else {
        ProxyAccessMode::Automatic
    }
}

#[cfg(windows)]
fn stream_winhttp(
    config: &OpenAiConfig,
    body: Vec<u8>,
    cancellation: CancellationToken,
    sink: &mut dyn DeltaSink,
    max_output_tokens: Option<u32>,
) -> ProviderResult {
    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::*;

    let parsed = validate_url(&config.base_url)?;
    let endpoint = chat_completions_path(&parsed.path);
    let endpoint_w = wide(&endpoint);
    let host_w = wide(&parsed.host);
    let agent_w = wide("SelectionTranslate/0.1");
    let method_w = wide("POST");
    let timeout_ms = config.timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe {
        let access_type = match proxy_access_mode(&parsed) {
            ProxyAccessMode::Direct => WINHTTP_ACCESS_TYPE_NO_PROXY,
            ProxyAccessMode::Automatic => WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
        };
        let session = WinHttpOpen(
            PCWSTR(agent_w.as_ptr()),
            access_type,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return Err(ProviderError::Transport);
        }
        let result = (|| {
            WinHttpSetTimeouts(session, timeout_ms, timeout_ms, timeout_ms, timeout_ms)
                .map_err(win_error)?;
            if cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let connection = WinHttpConnect(session, PCWSTR(host_w.as_ptr()), parsed.port, 0);
            if connection.is_null() {
                return Err(ProviderError::Transport);
            }
            let result = (|| {
                let flags = if parsed.secure {
                    WINHTTP_FLAG_SECURE
                } else {
                    WINHTTP_OPEN_REQUEST_FLAGS(0)
                };
                let request = WinHttpOpenRequest(
                    connection,
                    PCWSTR(method_w.as_ptr()),
                    PCWSTR(endpoint_w.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    flags,
                );
                if request.is_null() {
                    return Err(ProviderError::Transport);
                }
                cancellation.set_active(request);
                let result = (|| {
                    if cancellation.is_cancelled() {
                        return Err(ProviderError::Cancelled);
                    }
                    // WinHTTP copies these headers into the request. Keep the
                    // dedicated buffers alive for the complete API call, then
                    // wipe them as soon as the call returns.
                    {
                        let headers = SensitiveHeaders::new(config.api_key.as_deref());
                        WinHttpAddRequestHeaders(
                            request,
                            headers.as_slice(),
                            WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
                        )
                        .map_err(|error| win_error_or_cancel(error, &cancellation))?;
                    }
                    WinHttpSendRequest(
                        request,
                        None,
                        Some(body.as_ptr() as *const core::ffi::c_void),
                        body.len() as u32,
                        body.len() as u32,
                        0,
                    )
                    .map_err(|error| win_error_or_cancel(error, &cancellation))?;
                    WinHttpReceiveResponse(request, std::ptr::null_mut())
                        .map_err(|error| win_error_or_cancel(error, &cancellation))?;
                    if cancellation.is_cancelled() {
                        return Err(ProviderError::Cancelled);
                    }
                    let mut status = 0u32;
                    let mut status_len = std::mem::size_of::<u32>() as u32;
                    WinHttpQueryHeaders(
                        request,
                        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                        PCWSTR::null(),
                        Some((&mut status as *mut u32).cast()),
                        &mut status_len,
                        std::ptr::null_mut(),
                    )
                    .map_err(|error| win_error_or_cancel(error, &cancellation))?;
                    if status >= 400 {
                        return Err(if status == 429 {
                            ProviderError::RateLimited
                        } else {
                            ProviderError::HttpStatus(status as u16)
                        });
                    }
                    read_response(request, &cancellation, sink, max_output_tokens)
                })();
                if cancellation.clear_active(request) {
                    let _ = WinHttpCloseHandle(request);
                }
                result
            })();
            let _ = WinHttpCloseHandle(connection);
            result
        })();
        let _ = WinHttpCloseHandle(session);
        result
    }
}

#[cfg(windows)]
fn read_response(
    request: *mut core::ffi::c_void,
    cancellation: &CancellationToken,
    sink: &mut dyn DeltaSink,
    max_output_tokens: Option<u32>,
) -> ProviderResult {
    use windows::Win32::Networking::WinHttp::{WinHttpQueryDataAvailable, WinHttpReadData};
    let mut decoder = sse::Decoder::default();
    let mut mode: Option<bool> = None; // true = SSE, false = JSON
    let mut prefix = Vec::new();
    let mut json_body = Vec::new();
    let mut done = false;
    let mut response_bytes = 0usize;
    let mut output_bytes = 0usize;
    let max_output_bytes = output_byte_limit(max_output_tokens);
    loop {
        if cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut available = 0u32;
        unsafe {
            WinHttpQueryDataAvailable(request, &mut available)
                .map_err(|error| win_error_or_cancel(error, cancellation))?;
        }
        if available == 0 {
            break;
        }
        let read_size = available.min(MAX_READ_BYTES);
        let mut buffer = vec![0u8; read_size as usize];
        let mut read = 0u32;
        unsafe {
            WinHttpReadData(request, buffer.as_mut_ptr().cast(), read_size, &mut read)
                .map_err(|error| win_error_or_cancel(error, cancellation))?;
        }
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read as usize];
        response_bytes = response_bytes
            .checked_add(chunk.len())
            .filter(|size| *size <= MAX_RESPONSE_BYTES)
            .ok_or(ProviderError::ResponseTooLarge)?;
        if mode.is_none() {
            prefix.extend_from_slice(chunk);
            let first = prefix
                .iter()
                .copied()
                .find(|byte| !byte.is_ascii_whitespace());
            if first == Some(b'{') || first == Some(b'[') {
                mode = Some(false);
                json_body.extend_from_slice(&prefix);
                prefix.clear();
            } else if is_sse_prefix(&prefix) {
                mode = Some(true);
                for event in decoder.push(&prefix)? {
                    done |= emit_event(event, sink, &mut output_bytes, max_output_bytes)?;
                }
                prefix.clear();
                if done {
                    break;
                }
            } else if prefix.len() > 64 * 1024 {
                return Err(ProviderError::MalformedJson);
            }
        } else if mode == Some(false) {
            json_body.extend_from_slice(chunk);
        } else {
            for event in decoder.push(chunk)? {
                done |= emit_event(event, sink, &mut output_bytes, max_output_bytes)?;
            }
            if done {
                break;
            }
        }
    }
    if mode == Some(true) {
        if !done {
            for event in decoder.finish()? {
                if emit_event(event, sink, &mut output_bytes, max_output_bytes)? {
                    done = true;
                    break;
                }
            }
        }
        if done {
            Ok(())
        } else {
            Err(ProviderError::IncompleteResponse)
        }
    } else {
        if mode.is_none() {
            json_body.extend_from_slice(&prefix);
        }
        let output = sse::parse_non_streaming(&json_body)?;
        if output.len() > max_output_bytes {
            return Err(ProviderError::ResponseTooLarge);
        }
        if !output.is_empty() {
            sink.on_delta(&output);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn is_sse_prefix(bytes: &[u8]) -> bool {
    let Some(line_end) = bytes.iter().position(|byte| *byte == b'\n') else {
        return bytes.starts_with(b"data:")
            || bytes.starts_with(b":")
            || bytes.starts_with(b"event:");
    };
    let line = &bytes[..line_end];
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.starts_with(b"data:") || line.starts_with(b":") || line.starts_with(b"event:")
}

#[cfg(windows)]
fn emit_event(
    event: sse::Event,
    sink: &mut dyn DeltaSink,
    output_bytes: &mut usize,
    max_output_bytes: usize,
) -> ProviderResult<bool> {
    match event {
        sse::Event::Delta(delta) => {
            *output_bytes = output_bytes
                .checked_add(delta.len())
                .filter(|size| *size <= max_output_bytes)
                .ok_or(ProviderError::ResponseTooLarge)?;
            sink.on_delta(&delta);
            Ok(false)
        }
        sse::Event::Done => Ok(true),
    }
}

fn output_byte_limit(max_output_tokens: Option<u32>) -> usize {
    max_output_tokens
        .map(|tokens| {
            (tokens as usize)
                .saturating_mul(OUTPUT_BYTES_PER_TOKEN)
                .saturating_add(OUTPUT_BYTE_HEADROOM)
        })
        .unwrap_or(MAX_OUTPUT_BYTES)
        .min(MAX_OUTPUT_BYTES)
}

fn contains_unsafe_header_character(value: &str) -> bool {
    value.bytes().any(|byte| byte <= 0x1f || byte == 0x7f)
}

fn zeroize_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // Volatile writes prevent an optimizing compiler from removing the
        // wipe as dead because the buffer is about to be dropped.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

#[cfg(windows)]
fn zeroize_wide(values: &mut [u16]) {
    for value in values {
        unsafe { std::ptr::write_volatile(value, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

#[cfg(windows)]
struct SensitiveHeaders {
    utf8: Vec<u8>,
    utf16: Vec<u16>,
}

#[cfg(windows)]
impl SensitiveHeaders {
    fn new(api_key: Option<&str>) -> Self {
        const COMMON: &[u8] =
            b"Content-Type: application/json\r\nAccept: text/event-stream, application/json\r\n";
        const AUTH_PREFIX: &[u8] = b"Authorization: Bearer ";
        const LINE_END: &[u8] = b"\r\n";

        let key_len = api_key.map_or(0, str::len);
        let mut utf8 = Vec::with_capacity(
            COMMON
                .len()
                .saturating_add((key_len > 0) as usize * (AUTH_PREFIX.len() + LINE_END.len()))
                .saturating_add(key_len),
        );
        utf8.extend_from_slice(COMMON);
        if let Some(api_key) = api_key {
            utf8.extend_from_slice(AUTH_PREFIX);
            utf8.extend_from_slice(api_key.as_bytes());
            utf8.extend_from_slice(LINE_END);
        }

        let mut utf16 = Vec::with_capacity(utf8.len());
        push_wide_ascii(COMMON, &mut utf16);
        if let Some(api_key) = api_key {
            push_wide_ascii(AUTH_PREFIX, &mut utf16);
            utf16.extend(api_key.encode_utf16());
            push_wide_ascii(LINE_END, &mut utf16);
        }

        Self { utf8, utf16 }
    }

    fn as_slice(&self) -> &[u16] {
        &self.utf16
    }
}

#[cfg(windows)]
impl Drop for SensitiveHeaders {
    fn drop(&mut self) {
        zeroize_bytes(&mut self.utf8);
        zeroize_wide(&mut self.utf16);
    }
}

#[cfg(windows)]
fn push_wide_ascii(bytes: &[u8], destination: &mut Vec<u16>) {
    debug_assert!(bytes.is_ascii());
    destination.extend(bytes.iter().map(|byte| *byte as u16));
}

#[cfg(windows)]
fn win_error(error: windows::core::Error) -> ProviderError {
    ProviderError::from_winhttp(error.code().0 as u32)
}

#[cfg(windows)]
fn win_error_or_cancel(
    error: windows::core::Error,
    cancellation: &CancellationToken,
) -> ProviderError {
    if cancellation.is_timed_out() {
        ProviderError::Timeout
    } else if cancellation.is_cancelled() {
        ProviderError::Cancelled
    } else {
        win_error(error)
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        chat_completions_path, is_loopback, parse_url, proxy_access_mode, validate_url,
        ProxyAccessMode,
    };
    use crate::ProviderError;

    #[test]
    fn payload_uses_gate_rendering_and_profile_inference() {
        use super::{build_request_body, payload_from_request};
        use selection_core::{
            prepare_request, ExtractionSource, JobInput, PromptConfig, ProviderConfig, TextContext,
            TriggerKind,
        };

        let input = JobInput::new(
            1,
            TriggerKind::Selection,
            TextContext {
                target: "term".to_owned(),
                context: Some("A sentence containing term.".to_owned()),
                source: ExtractionSource::UiaSelection,
                screen_rect: None,
            },
            "profile",
        );
        let mut first_profile = PromptConfig::with_template(
            "profile",
            "Translate naturally.",
            "Target={target}; context={context}; source={source}",
        );
        first_profile.model = Some("model-a".to_owned());
        first_profile.temperature = Some(0.1);
        first_profile.max_output_tokens = Some(100);
        let first = prepare_request(
            &input,
            1,
            false,
            &[first_profile],
            Some(&ProviderConfig::new(
                "https://example.invalid/v1",
                "default-model",
            )),
        )
        .unwrap();

        let mut second_profile = PromptConfig::with_template(
            "profile",
            "Explain precisely.",
            "Explain={target}; context={context}; source={source}",
        );
        second_profile.model = Some("model-b".to_owned());
        second_profile.temperature = Some(0.8);
        second_profile.max_output_tokens = Some(200);
        let second = prepare_request(
            &input,
            1,
            false,
            &[second_profile],
            Some(&ProviderConfig::new(
                "https://example.invalid/v1",
                "default-model",
            )),
        )
        .unwrap();

        let first_body = build_request_body(&payload_from_request(&first)).unwrap();
        let second_body = build_request_body(&payload_from_request(&second)).unwrap();
        assert_ne!(first_body, second_body);
        let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(first_json["model"], "model-a");
        let first_system = first_json["messages"][0]["content"]
            .as_str()
            .expect("system message is text");
        assert!(first_system.starts_with("Translate naturally."));
        assert!(first_system.contains("Return the response as valid, concise Markdown."));
        assert_eq!(
            first_json["messages"][1]["content"],
            "Target=term; context=A sentence containing term.; source=UiaSelection"
        );
        let first_temperature = first_json["temperature"].as_f64().unwrap();
        assert!((first_temperature - 0.1).abs() < 1e-6);
        assert_eq!(first_json["max_tokens"], 100);
        assert!(first_json.get("max_output_tokens").is_none());
        assert!(first_json.get("enable_thinking").is_none());
        let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
        assert_eq!(second_json["model"], "model-b");
        let second_temperature = second_json["temperature"].as_f64().unwrap();
        assert!((second_temperature - 0.8).abs() < 1e-6);
        assert_eq!(second_json["max_tokens"], 200);
    }

    #[test]
    fn qwen_requests_explicitly_disable_thinking_without_affecting_other_models() {
        use super::{build_request_body, RequestPayload};

        let payload = RequestPayload {
            job_id: 1,
            system_prompt: "Translate.".to_owned(),
            user_prompt: "Text".to_owned(),
            model: " QwEn3.7-flash-2026-07-15".to_owned(),
            temperature: None,
            max_output_tokens: Some(32),
        };
        let body = build_request_body(&payload).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["enable_thinking"], false);

        let other = RequestPayload {
            model: "gpt-compatible".to_owned(),
            ..payload
        };
        let body = build_request_body(&other).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("enable_thinking").is_none());
    }

    #[test]
    fn only_https_or_loopback_http_is_allowed() {
        assert!(validate_url("https://api.example.test").is_ok());
        assert!(validate_url("http://127.0.0.1:1234").is_ok());
        assert!(validate_url("http://localhost:1234").is_ok());
        assert_eq!(
            validate_url("http://example.test"),
            Err(ProviderError::UnsupportedScheme)
        );
    }

    #[test]
    fn parses_paths_ports_and_loopback() {
        let parsed = parse_url("http://[::1]:8080/base").unwrap();
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 8080);
        assert_eq!(parsed.path, "/base");
        assert!(is_loopback("::1"));
    }

    #[test]
    fn joins_host_only_and_versioned_provider_base_paths_exactly_once() {
        for (base, expected) in [
            ("/", "/v1/chat/completions"),
            ("", "/v1/chat/completions"),
            ("/v1", "/v1/chat/completions"),
            ("/v1/", "/v1/chat/completions"),
            (
                "/compatible-mode/v1",
                "/compatible-mode/v1/chat/completions",
            ),
            (
                "/compatible-mode/v1/",
                "/compatible-mode/v1/chat/completions",
            ),
            (
                "/compatible-mode/v1/chat/completions",
                "/compatible-mode/v1/chat/completions",
            ),
            ("/gateway", "/gateway/v1/chat/completions"),
        ] {
            assert_eq!(chat_completions_path(base), expected, "base path: {base}");
        }
    }

    #[test]
    fn proxy_access_mode_keeps_loopback_direct_and_remote_configured() {
        let local_http = parse_url("http://127.0.0.1:1234").unwrap();
        let local_ipv6 = parse_url("http://[::1]:1234").unwrap();
        let local_name = parse_url("http://localhost:1234").unwrap();
        let remote_https = parse_url("https://api.example.test/v1").unwrap();

        assert_eq!(proxy_access_mode(&local_http), ProxyAccessMode::Direct);
        assert_eq!(proxy_access_mode(&local_ipv6), ProxyAccessMode::Direct);
        assert_eq!(proxy_access_mode(&local_name), ProxyAccessMode::Direct);
        assert_eq!(proxy_access_mode(&remote_https), ProxyAccessMode::Automatic);
    }

    #[test]
    fn rejects_ambiguous_authorities_and_queries() {
        for value in [
            "http://127.0.0.1.evil:8080",
            "http://[::1]junk:8080",
            "http://::1:8080",
            "http://localhost:0",
            "https://api.example.test/v1?token=secret",
        ] {
            assert!(validate_url(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn rejects_unsafe_api_key_header_values() {
        for key in [
            "bad\rkey",
            "bad\nkey",
            "bad\0key",
            "bad\u{001f}key",
            "bad\u{007f}key",
        ] {
            let error = super::OpenAiProvider::new(super::OpenAiConfig {
                api_key: Some(key.to_owned()),
                ..super::OpenAiConfig::default()
            })
            .err()
            .unwrap();
            assert_eq!(error, ProviderError::InvalidHeader);
            assert!(!error.to_string().contains(key));
        }
    }

    #[test]
    fn rejects_empty_or_whitespace_api_keys() {
        for key in ["", " ", "\t", "\n", "\u{2003}"] {
            let error = super::OpenAiProvider::new(super::OpenAiConfig {
                api_key: Some(key.to_owned()),
                ..super::OpenAiConfig::default()
            })
            .err()
            .unwrap();
            assert_eq!(error, ProviderError::InvalidConfiguration("api_key"));
        }
    }

    #[test]
    fn zeroize_helper_erases_every_byte() {
        let mut bytes = [0xde, 0xad, 0xbe, 0xef];
        super::zeroize_bytes(&mut bytes);
        assert_eq!(bytes, [0; 4]);
    }

    #[cfg(windows)]
    mod local_mock {
        use super::super::{CancellationToken, DeltaSink, OpenAiConfig, OpenAiProvider};
        use crate::ProviderError;
        use selection_core::{
            prepare_request, ExtractionSource, JobInput, PromptConfig, ProviderConfig, TextContext,
            TriggerKind,
        };
        use std::io::{ErrorKind, Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::{Duration, Instant};

        struct SharedSink(Arc<Mutex<Vec<String>>>);

        impl DeltaSink for SharedSink {
            fn on_delta(&mut self, delta: &str) {
                self.0.lock().unwrap().push(delta.to_owned());
            }
        }

        struct MockResponse {
            status: u16,
            content_type: &'static str,
            chunks: Vec<Vec<u8>>,
            delay: Option<Duration>,
            delay_after_headers: bool,
        }

        fn mock(response: MockResponse) -> (String, thread::JoinHandle<Vec<u8>>) {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let handle = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            break stream;
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return Vec::new();
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return Vec::new(),
                    }
                };
                let request = read_request(&mut stream);
                if !response.delay_after_headers {
                    if let Some(delay) = response.delay {
                        thread::sleep(delay);
                    }
                }
                let body_len: usize = response.chunks.iter().map(Vec::len).sum();
                let reason = match response.status {
                    200 => "OK",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    _ => "Error",
                };
                let headers = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n",
                    response.status, response.content_type
                );
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.flush();
                if response.delay_after_headers {
                    if let Some(delay) = response.delay {
                        thread::sleep(delay);
                    }
                }
                for chunk in response.chunks {
                    let _ = stream.write_all(&chunk);
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(10));
                }
                request
            });
            (endpoint, handle)
        }

        fn read_request(stream: &mut TcpStream) -> Vec<u8> {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0u8; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let Ok(count) = stream.read(&mut buf) else {
                    break;
                };
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..count]);
                if bytes.len() > 1_000_000 {
                    break;
                }
            }
            bytes
        }

        fn prepared(endpoint: &str) -> selection_core::PreparedRequest {
            prepared_with_max_output_tokens(endpoint, None)
        }

        fn prepared_with_max_output_tokens(
            endpoint: &str,
            max_output_tokens: Option<u32>,
        ) -> selection_core::PreparedRequest {
            let input = JobInput::new(
                42,
                TriggerKind::Selection,
                TextContext::new("hello", ExtractionSource::UiaSelection),
                "translation",
            );
            let mut prompt = PromptConfig::new("translation");
            prompt.max_output_tokens = max_output_tokens;
            prepare_request(
                &input,
                42,
                false,
                &[prompt],
                Some(&ProviderConfig::new(endpoint, "local-model")),
            )
            .unwrap()
        }

        fn provider(endpoint: &str, timeout: Duration) -> OpenAiProvider {
            OpenAiProvider::new(OpenAiConfig {
                base_url: endpoint.to_owned(),
                default_model: "local-model".to_owned(),
                timeout,
                api_key: None,
            })
            .unwrap()
        }

        #[test]
        fn winhttp_sends_the_versioned_base_path_without_a_duplicate_v1() {
            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: vec![b"data: [DONE]\n\n".to_vec()],
                delay: None,
                delay_after_headers: false,
            });
            let versioned_endpoint = format!("{endpoint}/compatible-mode/v1");
            let result = provider(&versioned_endpoint, Duration::from_secs(2)).stream(
                &prepared(&versioned_endpoint),
                CancellationToken::new(),
                SharedSink(Arc::new(Mutex::new(Vec::new()))),
            );
            let request = server.join().unwrap();
            assert_eq!(result, Ok(()));
            let request_line = request.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
            assert_eq!(
                request_line,
                b"POST /compatible-mode/v1/chat/completions HTTP/1.1\r"
            );
        }

        #[test]
        fn streams_local_sse_and_handles_split_utf8() {
            let payload = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xE4\xBD\xA0\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            let split = payload.iter().position(|byte| *byte == 0xA0).unwrap();
            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: vec![payload[..split].to_vec(), payload[split..].to_vec()],
                delay: None,
                delay_after_headers: false,
            });
            let request = prepared(&endpoint);
            let output = Arc::new(Mutex::new(Vec::new()));
            let result = provider(&endpoint, Duration::from_secs(2)).stream(
                &request,
                CancellationToken::new(),
                SharedSink(Arc::clone(&output)),
            );
            server.join().unwrap();
            assert_eq!(result, Ok(()));
            assert_eq!(&*output.lock().unwrap(), &["你", "ok"]);
        }

        #[test]
        fn falls_back_to_non_streaming_json() {
            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "application/json",
                chunks: vec![br#"{"choices":[{"message":{"content":"answer"}}]}"#.to_vec()],
                delay: None,
                delay_after_headers: false,
            });
            let output = Arc::new(Mutex::new(Vec::new()));
            let result = provider(&endpoint, Duration::from_secs(2)).stream(
                &prepared(&endpoint),
                CancellationToken::new(),
                SharedSink(Arc::clone(&output)),
            );
            server.join().unwrap();
            assert_eq!(result, Ok(()));
            assert_eq!(&*output.lock().unwrap(), &["answer"]);
        }

        #[test]
        fn rejects_truncated_sse_without_done_sentinel() {
            let payload = b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: vec![payload.to_vec()],
                delay: None,
                delay_after_headers: false,
            });
            let output = Arc::new(Mutex::new(Vec::new()));
            let result = provider(&endpoint, Duration::from_secs(2)).stream(
                &prepared(&endpoint),
                CancellationToken::new(),
                SharedSink(Arc::clone(&output)),
            );
            server.join().unwrap();
            assert_eq!(result, Err(ProviderError::IncompleteResponse));
            assert_eq!(&*output.lock().unwrap(), &["partial"]);
        }

        #[test]
        fn rejects_decoded_output_over_profile_limit() {
            let content = "x".repeat(2_000);
            let payload = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n\n"
            );
            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: vec![payload.into_bytes()],
                delay: None,
                delay_after_headers: false,
            });
            let output = Arc::new(Mutex::new(Vec::new()));
            let result = provider(&endpoint, Duration::from_secs(2)).stream(
                &prepared_with_max_output_tokens(&endpoint, Some(1)),
                CancellationToken::new(),
                SharedSink(Arc::clone(&output)),
            );
            server.join().unwrap();
            assert_eq!(result, Err(ProviderError::ResponseTooLarge));
            assert!(output.lock().unwrap().is_empty());
        }

        #[test]
        fn maps_http_and_timeout_failures_without_body_details() {
            for (status, expected) in [
                (401, ProviderError::HttpStatus(401)),
                (429, ProviderError::RateLimited),
                (500, ProviderError::HttpStatus(500)),
            ] {
                let (endpoint, server) = mock(MockResponse {
                    status,
                    content_type: "application/json",
                    chunks: vec![b"private response body".to_vec()],
                    delay: None,
                    delay_after_headers: false,
                });
                let result = provider(&endpoint, Duration::from_secs(2)).stream(
                    &prepared(&endpoint),
                    CancellationToken::new(),
                    SharedSink(Arc::new(Mutex::new(Vec::new()))),
                );
                server.join().unwrap();
                assert_eq!(result, Err(expected));
            }

            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: vec![b"x".to_vec()],
                delay: Some(Duration::from_secs(2)),
                delay_after_headers: true,
            });
            let started = Instant::now();
            let result = provider(&endpoint, Duration::from_millis(100)).stream(
                &prepared(&endpoint),
                CancellationToken::new(),
                SharedSink(Arc::new(Mutex::new(Vec::new()))),
            );
            server.join().unwrap();
            assert_eq!(result, Err(ProviderError::Timeout));
            assert!(started.elapsed() < Duration::from_secs(3));
        }

        #[test]
        fn cancellation_closes_the_active_request() {
            let (endpoint, server) = mock(MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: Vec::new(),
                delay: Some(Duration::from_millis(750)),
                delay_after_headers: false,
            });
            let provider = provider(&endpoint, Duration::from_secs(2));
            let request = prepared(&endpoint);
            let token = CancellationToken::new();
            let worker_token = token.clone();
            let worker = thread::spawn(move || {
                provider.stream(
                    &request,
                    worker_token,
                    SharedSink(Arc::new(Mutex::new(Vec::new()))),
                )
            });
            thread::sleep(Duration::from_millis(100));
            token.cancel();
            let result = worker.join().unwrap();
            server.join().unwrap();
            assert_eq!(result, Err(ProviderError::Cancelled));
        }
    }
}
