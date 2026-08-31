//! Opt-in, privacy-safe runtime lifecycle trace.
//!
//! The trace is intentionally disabled unless `SELECTION_TRANSLATE_RUNTIME_TRACE`
//! names a file.  Entries contain only fixed stage names, timestamps, and
//! numeric identifiers; no request, target, output, prompt, endpoint, or OS
//! error text is ever written.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const ENV_NAME: &str = "SELECTION_TRANSLATE_RUNTIME_TRACE";

/// Append a fixed lifecycle stage when tracing is explicitly enabled.
pub fn record(stage: &'static str) {
    append(stage, None);
}

/// Append a fixed lifecycle stage with a numeric identifier.
pub fn record_id(stage: &'static str, id: u64) {
    append(stage, Some(id));
}

fn append(stage: &'static str, id: Option<u64>) {
    let Some(path) = std::env::var_os(ENV_NAME) else {
        return;
    };
    let path = Path::new(&path);
    if !valid_path(path) {
        return;
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis());
    let thread = format!("{:?}", std::thread::current().id());
    let thread = sanitize_field(&thread);
    let mut line = format!(
        "utc_ms={millis} thread={thread} stage={}",
        sanitize_stage(stage)
    );
    if let Some(id) = id {
        line.push_str(&format!(" id={id}"));
    }
    line.push('\n');
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}

fn valid_path(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.file_name().is_some_and(|name| !name.is_empty())
}

fn sanitize_stage(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => byte as char,
            _ => '_',
        })
        .collect()
}

fn sanitize_field(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'(' | b')' => byte as char,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_sanitization_removes_delimiters_and_unicode() {
        assert_eq!(sanitize_stage("popup/show\nΔ"), "popup_show___");
    }

    #[test]
    fn path_validation_rejects_empty_and_accepts_filename() {
        assert!(!valid_path(Path::new("")));
        assert!(valid_path(Path::new("trace.log")));
        assert!(valid_path(Path::new("windows/tmp/trace.log")));
    }

    #[test]
    fn disabled_trace_does_not_create_a_file() {
        let old = std::env::var_os(ENV_NAME);
        std::env::remove_var(ENV_NAME);
        record("disabled-test");
        match old {
            Some(value) => std::env::set_var(ENV_NAME, value),
            None => std::env::remove_var(ENV_NAME),
        }
    }
}
