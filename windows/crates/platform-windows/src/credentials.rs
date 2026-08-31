//! Windows Credential Manager access for provider API keys.
//!
//! The configuration stores only a target name.  Secret material is kept in
//! the per-user Windows Credential Manager and is never logged or serialized
//! by this adapter.

use std::fmt;

const MAX_TARGET_UNITS: usize = 512;
const MAX_SECRET_BYTES: usize = 2560;

/// Errors returned by the platform credential store.
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// The target or secret does not meet the Credential Manager contract.
    InvalidInput(&'static str),
    /// A credential exists but its blob is not a valid UTF-8 API key.
    InvalidStoredValue,
    /// A Windows API call failed.  Only the operation and numeric error code
    /// are retained; no target or secret is included in diagnostics.
    Platform { operation: &'static str, code: i32 },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(formatter, "invalid credential input: {reason}"),
            Self::InvalidStoredValue => formatter.write_str("stored credential is not valid UTF-8"),
            Self::Platform { operation, code } => {
                write!(
                    formatter,
                    "credential operation {operation} failed ({code})"
                )
            }
        }
    }
}

impl std::error::Error for CredentialError {}

fn validate_target(target: &str) -> Result<(), CredentialError> {
    if target.trim().is_empty() {
        return Err(CredentialError::InvalidInput("target is empty"));
    }
    if target.chars().any(|character| character == '\0') {
        return Err(CredentialError::InvalidInput("target contains NUL"));
    }
    if target.encode_utf16().count() > MAX_TARGET_UNITS {
        return Err(CredentialError::InvalidInput("target is too long"));
    }
    Ok(())
}

fn validate_secret(secret: &str) -> Result<(), CredentialError> {
    if secret.is_empty() {
        return Err(CredentialError::InvalidInput("secret is empty"));
    }
    if secret.as_bytes().contains(&0) {
        return Err(CredentialError::InvalidInput("secret contains NUL"));
    }
    if secret.len() > MAX_SECRET_BYTES {
        return Err(CredentialError::InvalidInput("secret is too long"));
    }
    Ok(())
}

/// Store an API key as a generic, locally persistent Windows credential.
pub fn write_api_key(target: &str, api_key: &str) -> Result<(), CredentialError> {
    validate_target(target)?;
    validate_secret(api_key)?;
    write_api_key_impl(target, api_key)
}

/// Read an API key.  `Ok(None)` means that the target does not exist.
pub fn read_api_key(target: &str) -> Result<Option<String>, CredentialError> {
    validate_target(target)?;
    read_api_key_impl(target)
}

/// Delete an API key.  Deleting an already absent target succeeds.
pub fn delete_api_key(target: &str) -> Result<(), CredentialError> {
    validate_target(target)?;
    delete_api_key_impl(target)
}

#[cfg(windows)]
fn platform_error(operation: &'static str, error: windows::core::Error) -> CredentialError {
    CredentialError::Platform {
        operation,
        code: error.code().0,
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::ptr;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    struct ZeroizingWide(Vec<u16>);

    impl ZeroizingWide {
        fn from_str(value: &str) -> Self {
            let mut units: Vec<u16> = value.encode_utf16().collect();
            units.push(0);
            Self(units)
        }

        fn as_pcwstr(&self) -> PCWSTR {
            PCWSTR(self.0.as_ptr())
        }
    }

    impl Drop for ZeroizingWide {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }

    struct CredentialGuard(*mut CREDENTIALW);

    impl Drop for CredentialGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // CredRead allocates the complete structure and blob in one
                // buffer; CredFree is required for every successful read.
                unsafe { CredFree(self.0.cast()) };
            }
        }
    }

    pub(super) fn write_api_key_impl(target: &str, api_key: &str) -> Result<(), CredentialError> {
        let target = ZeroizingWide::from_str(target);
        let blob = api_key.as_bytes();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.0.as_ptr() as *mut u16),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        unsafe { CredWriteW(&credential, 0) }.map_err(|error| platform_error("write", error))
    }

    pub(super) fn read_api_key_impl(target: &str) -> Result<Option<String>, CredentialError> {
        let target = ZeroizingWide::from_str(target);
        let mut raw = ptr::null_mut();
        match unsafe { CredReadW(target.as_pcwstr(), CRED_TYPE_GENERIC, None, &mut raw) } {
            Ok(()) => {}
            Err(error) if error.code().0 == 1168 => return Ok(None), // ERROR_NOT_FOUND
            Err(error) => return Err(platform_error("read", error)),
        }
        let credential = CredentialGuard(raw);
        if credential.0.is_null() {
            return Err(CredentialError::Platform {
                operation: "read",
                code: 998, // ERROR_NOACCESS; defensive null-result handling.
            });
        }
        let credential = unsafe { &*credential.0 };
        let size = credential.CredentialBlobSize as usize;
        if size == 0 || size > MAX_SECRET_BYTES || credential.CredentialBlob.is_null() {
            return Err(CredentialError::InvalidStoredValue);
        }
        // Copy only the bytes needed to construct the caller-owned String.
        // The Credential Manager-owned blob is released by CredentialGuard.
        let bytes = unsafe { std::slice::from_raw_parts(credential.CredentialBlob, size) };
        let copy = bytes.to_vec();
        match String::from_utf8(copy) {
            Ok(secret) => Ok(Some(secret)),
            Err(error) => {
                let mut invalid_bytes = error.into_bytes();
                invalid_bytes.fill(0);
                Err(CredentialError::InvalidStoredValue)
            }
        }
    }

    pub(super) fn delete_api_key_impl(target: &str) -> Result<(), CredentialError> {
        let target = ZeroizingWide::from_str(target);
        match unsafe { CredDeleteW(target.as_pcwstr(), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if error.code().0 == 1168 => Ok(()), // ERROR_NOT_FOUND
            Err(error) => Err(platform_error("delete", error)),
        }
    }
}

#[cfg(windows)]
use windows_impl::{delete_api_key_impl, read_api_key_impl, write_api_key_impl};

#[cfg(not(windows))]
fn write_api_key_impl(_target: &str, _api_key: &str) -> Result<(), CredentialError> {
    Err(CredentialError::Platform {
        operation: "write",
        code: -1,
    })
}

#[cfg(not(windows))]
fn read_api_key_impl(_target: &str) -> Result<Option<String>, CredentialError> {
    Err(CredentialError::Platform {
        operation: "read",
        code: -1,
    })
}

#[cfg(not(windows))]
fn delete_api_key_impl(_target: &str) -> Result<(), CredentialError> {
    Err(CredentialError::Platform {
        operation: "delete",
        code: -1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_inputs_before_touching_the_store() {
        assert!(matches!(
            write_api_key("", "secret"),
            Err(CredentialError::InvalidInput("target is empty"))
        ));
        assert!(matches!(
            write_api_key("target", ""),
            Err(CredentialError::InvalidInput("secret is empty"))
        ));
        assert!(matches!(
            write_api_key("target\0suffix", "secret"),
            Err(CredentialError::InvalidInput("target contains NUL"))
        ));
        assert!(matches!(
            write_api_key("target", "secret\0suffix"),
            Err(CredentialError::InvalidInput("secret contains NUL"))
        ));
    }

    #[test]
    fn diagnostics_do_not_include_secret_material() {
        let error = CredentialError::InvalidInput("secret is empty");
        assert!(!error.to_string().contains("api-key-value"));
    }
}
