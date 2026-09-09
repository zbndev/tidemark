//! What to call a provider in front of a person.
//!
//! The daemon's catalog spells each name — "DeepSeek", "OpenRouter", "ClinePass" — and
//! `tidemark_types::provider_label` only capitalises a slug, so a client that printed the
//! label alone would say "Deepseek" where the card, the tray menu and the notification all
//! say "DeepSeek". The GTK window resolves this the same way in
//! `crates/tidemark/src/model.rs`, and the daemon does it again in `notify.rs`: catalog
//! first, label as the fallback for a provider newer than this build.

use std::collections::BTreeMap;

use tidemark_ipc::DaemonProxy;
use tidemark_types::ProviderDefinition;

use crate::exit::Failure;

/// The catalog's spelling of each provider's name, by slug.
#[derive(Debug, Default)]
pub struct Titles(BTreeMap<String, String>);

impl Titles {
    pub fn index(definitions: &[ProviderDefinition]) -> Self {
        Self(
            definitions
                .iter()
                .map(|definition| (definition.provider.clone(), definition.title.clone()))
                .collect(),
        )
    }

    /// One catalog read. Worth its round trip only for output a person reads: `--format
    /// json` names providers by slug and asks for none of this.
    pub async fn fetch(proxy: &DaemonProxy<'_>) -> Result<Self, Failure> {
        Ok(Self::index(&proxy.list_providers().await?))
    }

    /// The catalog's title when this build's daemon published one, and the slug's
    /// capitalisation when it did not — a newer daemon may watch a provider this client has
    /// never heard of, and its line is still worth printing.
    pub fn name(&self, slug: &str) -> String {
        self.0
            .get(slug)
            .cloned()
            .unwrap_or_else(|| tidemark_types::provider_label(slug))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(provider: &str, title: &str) -> ProviderDefinition {
        ProviderDefinition {
            provider: provider.to_owned(),
            title: title.to_owned(),
            credential: "key".to_owned(),
            credential_hint: String::new(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
            plugin: None,
        }
    }

    #[test]
    fn the_catalog_spelling_wins_over_the_capitalised_slug() {
        let titles = Titles::index(&[definition("deepseek", "DeepSeek")]);
        assert_eq!(titles.name("deepseek"), "DeepSeek");
    }

    /// A daemon newer than this client watches providers whose titles it never sent, and a
    /// line saying "Clinepass" beats no line at all.
    #[test]
    fn a_provider_the_catalog_never_mentioned_still_gets_a_name() {
        let titles = Titles::default();
        assert_eq!(titles.name("clinepass"), "Clinepass");
        assert_eq!(titles.name("zai"), "Z.ai");
    }
}
