//! Where a Chromium-family browser keeps the secret used to seal cookie values.
//!
//! Linux browsers file a Safe Storage password in Secret Service. Windows Chromium stores
//! a random AES-256 key in `Local State`, prefixed with `DPAPI` after base64 decoding; that
//! key is unprotected in the running user's DPAPI context. Neither path prompts or writes.

use crate::secrets::SecretError;

/// The password a browser sealed its cookie values with, if it stored one.
pub trait SafeStorage: std::fmt::Debug + Send + Sync {
    /// The Linux Safe Storage password for `application`. Windows Chromium does not use
    /// this value; its key is loaded from the selected profile's `Local State`.
    fn password(
        &self,
        application: &str,
    ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>>;
}

/// The operating system's browser secret storage.
#[derive(Debug, Default)]
pub struct Keyring;

#[cfg(unix)]
impl SafeStorage for Keyring {
    fn password(
        &self,
        application: &str,
    ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
        use oo7::dbus::Service;

        let application = application.to_owned();
        Box::pin(async move {
            let service = Service::new().await?;
            // The default collection only: prompting for a non-default one is not a
            // thing a background process may cause, and Chromium files its password here.
            let collection = service.default_collection().await?;
            if collection.is_locked().await? {
                return Err(SecretError::Locked);
            }
            let items = collection
                .search_items(&[("application", application.as_str())])
                .await?;
            let Some(item) = items.into_iter().next() else {
                return Ok(None);
            };
            let secret = item.secret().await?;
            let password = String::from_utf8(secret.to_vec()).map_err(|_| SecretError::NotUtf8)?;
            Ok(Some(password))
        })
    }
}

#[cfg(windows)]
impl SafeStorage for Keyring {
    fn password(
        &self,
        _application: &str,
    ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
        Box::pin(async { Ok(None) })
    }
}

#[cfg(windows)]
mod windows_dpapi {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;

    use base64::Engine as _;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    use super::super::CookieError;

    struct LocalAllocation(NonNull<u8>);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: the allocation was returned by DPAPI and remains owned here.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.as_ptr().cast::<c_void>()))) };
        }
    }

    pub(crate) fn key_for(cookie_database: &Path) -> Result<[u8; 32], CookieError> {
        let profile = cookie_database.parent().and_then(|parent| {
            if parent.file_name().is_some_and(|name| name == "Network") {
                parent.parent()
            } else {
                Some(parent)
            }
        });
        let Some(profile) = profile else {
            return Err(unavailable("the cookie database has no profile directory"));
        };
        let local_state = [
            profile.join("Local State"),
            profile.parent().unwrap_or(profile).join("Local State"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| unavailable("Local State is absent for this Chromium profile"))?;
        key_from_local_state(&local_state)
    }

    fn key_from_local_state(path: &Path) -> Result<[u8; 32], CookieError> {
        let bytes = std::fs::read(path).map_err(|error| unreadable(path, error))?;
        let document: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| unavailable("Local State is malformed JSON"))?;
        let encoded = document
            .pointer("/os_crypt/encrypted_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| unavailable("Local State has no os_crypt.encrypted_key"))?;
        let wrapped = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| unavailable("Local State encrypted_key is malformed base64"))?;
        let protected = wrapped
            .strip_prefix(b"DPAPI")
            .ok_or_else(|| unavailable("Local State encrypted_key has no DPAPI prefix"))?;
        let key = unprotect(protected)?;
        key.try_into()
            .map_err(|_| unavailable("the DPAPI-unprotected Chromium key is not 32 bytes"))
    }

    // A test-only QA hook: it exists so a fixture can produce DPAPI-shaped blobs, so it
    // is compiled only for tests (the one caller is a `#[cfg(windows)] #[test]`).
    #[cfg(test)]
    pub(crate) fn protect_for_test(input: &[u8]) -> Result<Vec<u8>, CookieError> {
        crypt(input, true)
    }

    fn unprotect(input: &[u8]) -> Result<Vec<u8>, CookieError> {
        crypt(input, false)
    }

    fn crypt(input: &[u8], protect: bool) -> Result<Vec<u8>, CookieError> {
        let length = u32::try_from(input.len())
            .map_err(|_| unavailable("the DPAPI browser key input is too large"))?;
        let input = CRYPT_INTEGER_BLOB {
            cbData: length,
            pbData: input.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let result = if protect {
            // SAFETY: the input points to `length` initialized bytes and output is writable.
            unsafe {
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    None,
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        } else {
            // SAFETY: the input points to `length` initialized bytes and output is writable.
            unsafe {
                CryptUnprotectData(
                    &input,
                    None,
                    None,
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        result.map_err(|_| unavailable("DPAPI could not unprotect this Chromium key"))?;
        let allocation = LocalAllocation(
            NonNull::new(output.pbData)
                .ok_or_else(|| unavailable("DPAPI returned a null browser-key allocation"))?,
        );
        let length = usize::try_from(output.cbData)
            .map_err(|_| unavailable("DPAPI returned an invalid browser-key length"))?;
        // SAFETY: DPAPI returned an allocation containing `cbData` initialized bytes.
        Ok(unsafe { std::slice::from_raw_parts(allocation.0.as_ptr(), length) }.to_vec())
    }

    fn unreadable(path: &Path, source: std::io::Error) -> CookieError {
        CookieError::Unreadable {
            path: PathBuf::from(path),
            source,
        }
    }

    fn unavailable(reason: &'static str) -> CookieError {
        CookieError::PlatformUnavailable(reason)
    }
}

#[cfg(windows)]
pub(crate) use windows_dpapi::key_for;
#[cfg(all(test, windows))]
pub(crate) use windows_dpapi::protect_for_test;
