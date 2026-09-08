//! The accounts one provider carries.
//!
//! `account rm` is deliberately the same daemon call as `provider rm`: there is one
//! removal, and which noun the user reached for does not change what happens to the
//! credential, the history or the card.

use tidemark_ipc::DaemonProxy;

use crate::cli::AccountCommand;
use crate::exit::{Exit, Failure};

pub async fn run(proxy: &DaemonProxy<'_>, command: AccountCommand) -> Result<Exit, Failure> {
    match command {
        AccountCommand::Add { provider, account } => proxy.add_account(&provider, &account).await?,
        AccountCommand::Rm { provider, account } => {
            proxy.remove_provider(&provider, &account).await?
        }
        AccountCommand::Rename {
            provider,
            account,
            new,
        } => proxy.rename_account(&provider, &account, &new).await?,
        AccountCommand::Order { provider, accounts } => {
            if accounts.is_empty() {
                return Err(Failure::usage(
                    "account order needs every account of that provider, in the order you want",
                ));
            }
            proxy.set_account_order(&provider, accounts).await?
        }
    }
    Ok(Exit::Ok)
}
