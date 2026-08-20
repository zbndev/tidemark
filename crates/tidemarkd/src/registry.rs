//! Which accounts this build watches.
//!
//! Registration is the whole of "adding a provider": one entry here, and the client itself
//! in `tidemark-core`. Nothing else in the daemon names a provider.
//!
//! There is no configuration file yet, and deliberately so — an account exists as far as
//! the daemon is concerned, and reports `no-credential` until a key for it turns up in the
//! Secret Service. That is the state the credentials UI is built to clear, and inventing a
//! config format before there is a dialog to write it would fix the shape of one against a
//! screen nobody has drawn.

use std::sync::Arc;

use tidemark_core::providers::{Provider, zai};
use tidemark_types::{AccountId, ProviderId};

use crate::engine::Account;

/// Every account the daemon polls.
pub fn accounts() -> Vec<Account> {
    vec![zai_account()]
}

fn zai_account() -> Account {
    Account::new(
        ProviderId::new(zai::PROVIDER_ID),
        AccountId::default(),
        Box::new(|credential| {
            // Region is fixed to Global until there is somewhere for the user to say
            // otherwise; that arrives with the credentials dialog, together with the key
            // entry itself. A BigModel CN key against this host answers 401, which the
            // interface already has a state for.
            Ok(Arc::new(zai::Zai::new(credential, zai::Region::Global)?) as Arc<dyn Provider>)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_account_is_published_before_it_is_polled() {
        let accounts = accounts();
        assert_eq!(accounts.len(), 1);
        let status = accounts[0].status();
        assert_eq!(status.provider, zai::PROVIDER_ID);
        assert_eq!(status.account, "default");
        assert!(status.captured_at.is_none());
    }
}
