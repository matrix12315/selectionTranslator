use std::fmt;

/// Failures returned by the local provider adapter.  Error values deliberately
/// contain no request or response content: they are safe to show in a local
/// diagnostic surface without leaking credentials or user text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    InvalidConfiguration(&'static str),
    UnsupportedScheme,
    InvalidUrl,
    InvalidRequest(&'static str),
    Dns,
    Tls,
    Timeout,
    Transport,
    HttpStatus(u16),
    RateLimited,
    MalformedJson,
    IncompleteResponse,
    ResponseTooLarge,
    InvalidHeader,
    Cancelled,
    UnsupportedPlatform,
}

impl ProviderError {
    pub(crate) fn from_winhttp(code: u32) -> Self {
        match code {
            12002 => Self::Timeout,     // ERROR_WINHTTP_TIMEOUT
            12005 | 12007 => Self::Dns, // INVALID_URL / NAME_NOT_RESOLVED
            12017 => Self::Cancelled,   // ERROR_WINHTTP_OPERATION_CANCELLED
            12157 | 12169..=12171 | 12175 => Self::Tls,
            12029..=12036 => Self::Transport,
            _ => Self::Transport,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::InvalidConfiguration(_) => "invalid provider configuration",
            Self::UnsupportedScheme => "provider URL must use HTTPS (or loopback HTTP)",
            Self::InvalidUrl => "invalid provider URL",
            Self::InvalidRequest(_) => "invalid prepared request",
            Self::Dns => "provider host could not be resolved",
            Self::Tls => "secure provider connection failed",
            Self::Timeout => "provider request timed out",
            Self::Transport => "provider connection failed",
            Self::HttpStatus(_) => "provider returned an HTTP error",
            Self::RateLimited => "provider rate limit exceeded",
            Self::MalformedJson => "provider returned malformed data",
            Self::IncompleteResponse => "provider response ended before completion",
            Self::ResponseTooLarge => "provider response exceeded the safety limit",
            Self::InvalidHeader => "provider request contains an invalid header value",
            Self::Cancelled => "provider request was cancelled",
            Self::UnsupportedPlatform => "provider is unavailable on this platform",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ProviderError {}

pub type ProviderResult<T = ()> = Result<T, ProviderError>;
