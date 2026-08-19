//! Vocabulary shared by every Tidemark process.
//!
//! This crate is the contract between the daemon and its clients: the GUI today, a CLI
//! later. It holds the domain vocabulary defined in `CONTEXT.md` — provider, account,
//! window, segment, snapshot, pace — and the shapes those travel in over D-Bus.
//!
//! It deliberately depends on nothing that talks to the network, the disk or the display.
//! Anything that needs an HTTP client belongs in `tidemark-core`; anything that needs GTK
//! belongs in `tidemark`. `scripts/check-layering.sh` enforces that.

pub mod snapshot;
pub mod time;
pub mod window;

pub use snapshot::{AccountId, DetailRow, DetailSection, ProviderId, Snapshot};
pub use time::{AbsurdTimestamp, Timestamp};
pub use window::{Window, WindowKey, WindowLength};

/// Identity constants. Changing any of these is a breaking change for installed units,
/// stored secrets and on-disk state, so they live in exactly one place.
pub mod ids {
    /// Application ID, and the bus name the GUI owns.
    pub const APP_ID: &str = "io.github.zbndev.Tidemark";
    /// Bus name the daemon owns.
    pub const DAEMON_BUS_NAME: &str = "io.github.zbndev.Tidemark.Daemon";
    /// Object path both processes agree on.
    pub const OBJECT_PATH: &str = "/io/github/zbndev/Tidemark";
    /// Secret Service schema for keys Tidemark owns.
    pub const SECRET_SCHEMA: &str = "io.github.zbndev.Tidemark.ProviderKey";
}

/// The value every outbound request identifies itself with.
///
/// Not cosmetic: `platform.claude.com` sits behind Cloudflare and answers a request with
/// no user agent with `403 browser_signature_banned`. See `CONTEXT.md` § Networking.
pub fn user_agent() -> String {
    format!("Tidemark/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn user_agent_names_the_product_and_a_version() {
        let ua = super::user_agent();
        assert!(ua.starts_with("Tidemark/"), "{ua}");
        assert!(ua.len() > "Tidemark/".len(), "{ua}");
    }
}
