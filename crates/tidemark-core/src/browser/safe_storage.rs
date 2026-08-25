//! Where a Chromium-family browser keeps the password its cookie values are sealed with.
//!
//! Chromium files a random "Safe Storage" password in the Secret Service under the
//! `application` attribute (`chrome`, `chromium`, `brave`, …) and the
//! `chrome_libsecret_os_crypt_password_v2` schema. That secret is not Tidemark's — it is
//! the browser's own, which is why it is not read through [`crate::secrets::Store`]: the
//! store is typed and attributed for the two schemas Tidemark files under, and a foreign
//! item is a different question with a different answer shape. What it shares is the
//! locked-collection rule: a locked keyring is a state the caller waits out, never a prompt
//! triggered from a background process.
//!
//! The seam is the [`SafeStorage`] trait so the cryptography in [`super::chromium`] is
//! testable against a fixed password rather than against whatever keyring a test machine
//! happens to run.

use oo7::dbus::Service;

use crate::secrets::SecretError;

/// The password a browser sealed its cookie values with, if it stored one.
pub trait SafeStorage: std::fmt::Debug + Send + Sync {
    /// The Safe Storage password for `application` — Chrome is `chrome`, Chromium is
    /// `chromium`, Brave is `brave`. `Ok(None)` when the browser never stored one, which
    /// is the world Chromium's `v10` fallback exists for.
    fn password(
        &self,
        application: &str,
    ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>>;
}

/// The Secret Service this machine is running.
#[derive(Debug, Default)]
pub struct Keyring;

impl SafeStorage for Keyring {
    fn password(
        &self,
        application: &str,
    ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
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
