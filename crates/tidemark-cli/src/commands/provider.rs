//! The provider catalog and the configured set.
//!
//! Nothing here decides anything: the daemon owns the config file, the credentials and the
//! order, and a second opinion living in the client is a second opinion that goes stale.
//! The one exception is an argument that cannot be a request at all — see `Order`.

use tidemark_ipc::DaemonProxy;

use crate::cli::ProviderCommand;
use crate::exit::{Exit, Failure};
use crate::titles::Titles;

pub async fn run(proxy: &DaemonProxy<'_>, command: ProviderCommand) -> Result<Exit, Failure> {
    match command {
        ProviderCommand::Catalog => {
            for definition in proxy.list_providers().await? {
                println!(
                    "{:<20} {:<24} {}",
                    definition.provider, definition.title, definition.credential
                );
            }
        }
        ProviderCommand::List => {
            let titles = Titles::fetch(proxy).await?;
            for status in proxy.get_status().await? {
                println!(
                    "{:<20} {:<12} {:<20} {}",
                    status.provider,
                    status.account,
                    titles.name(&status.provider),
                    status.state
                );
            }
        }
        ProviderCommand::Add { provider } => proxy.add_provider(&provider).await?,
        ProviderCommand::Rm { provider, account } => {
            proxy.remove_provider(&provider, &account).await?
        }
        // The daemon takes an order as a permutation of the configured set and refuses a
        // partial one; refusing an empty list here keeps a mistyped shell glob from
        // becoming a D-Bus error the user has to interpret.
        ProviderCommand::Order { providers } => {
            if providers.is_empty() {
                return Err(Failure::usage(
                    "provider order needs the whole configured set, in the order you want",
                ));
            }
            proxy.set_order(&providers).await?
        }
    }
    Ok(Exit::Ok)
}
