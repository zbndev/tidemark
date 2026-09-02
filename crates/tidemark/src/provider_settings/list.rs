use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use tidemark_types::{ProviderDefinition, ProviderStatus, provider_label};

use super::{model, multi_account_capable, opens_detail_after_add};
use crate::mark;

type IdentityCallback = Rc<dyn Fn(String, String)>;
type ProviderCallback = Rc<dyn Fn(String)>;

/// A provider and its accounts, in the order the list draws them.
#[derive(Debug, PartialEq)]
struct ProviderGroup {
    /// The provider slug, keying the provider's own row.
    provider: String,
    /// The provider's accounts in configured order. The first is drawn on the provider's
    /// own row — a provider with one account looks exactly as it always has — and the
    /// rest are nested rows under it.
    accounts: Vec<ProviderStatus>,
}

/// Groups a flat status list into providers: one entry per provider, in the order the
/// provider first appears, with that provider's accounts in the order they appear.
fn group(statuses: &[ProviderStatus]) -> Vec<ProviderGroup> {
    let mut groups: Vec<ProviderGroup> = Vec::new();
    for status in statuses {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.provider == status.provider)
        {
            group.accounts.push(status.clone());
        } else {
            groups.push(ProviderGroup {
                provider: status.provider.clone(),
                accounts: vec![status.clone()],
            });
        }
    }
    groups
}

/// The identity sequence the list draws: one provider key per provider, then one account
/// key per account beyond the first. Held rows are compared against it to decide whether
/// the picture changed only in words — updated in place, so a click never lands on a row
/// replaced under it — or in shape, which is drawn fresh.
fn structure(groups: &[ProviderGroup]) -> Vec<(&str, Option<&str>)> {
    groups
        .iter()
        .flat_map(|group| {
            std::iter::once((group.provider.as_str(), None)).chain(
                group.accounts[1..]
                    .iter()
                    .map(|status| (group.provider.as_str(), Some(status.account.as_str()))),
            )
        })
        .collect()
}

/// The configured rows on the dialog's main page.
#[derive(Debug)]
pub(super) struct ConfiguredList {
    pub(super) group: adw::PreferencesGroup,
    empty: adw::StatusPage,
    rows: RefCell<Vec<ConfiguredRow>>,
}

/// One drawn row of the configured list: a provider's own row, or one of the accounts
/// nested under it.
#[derive(Debug)]
struct ConfiguredRow {
    /// The provider every identity below belongs to.
    provider: String,
    /// The account the row is, or `None` for the provider's own row — which is the
    /// provider's first account's row in everything but name.
    account: Option<String>,
    row: adw::ActionRow,
    image: gtk::Image,
    /// The pen: the provider row's opens the provider's first account, a nested row's
    /// opens that account.
    edit: gtk::Button,
    /// The "+", on the provider row alone. `None` on nested rows, which are the accounts
    /// the "+" exists to create.
    add: Option<gtk::Button>,
}

impl ConfiguredList {
    pub(super) fn new(on_add: Rc<dyn Fn()>) -> Self {
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add provider")
            .valign(gtk::Align::Center)
            .build();
        add.connect_clicked(move |_| on_add());

        let group = adw::PreferencesGroup::builder()
            .title("Providers")
            .header_suffix(&add)
            .build();
        let empty = adw::StatusPage::builder()
            .title("No providers added")
            .description("Use + to add a provider.")
            .build();
        group.add(&empty);

        Self {
            group,
            empty,
            rows: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn apply(
        &self,
        definitions: &[ProviderDefinition],
        statuses: &[ProviderStatus],
        is_waiting: &dyn Fn(&str, &str) -> bool,
        on_edit: IdentityCallback,
        on_remove: IdentityCallback,
        on_add_account: ProviderCallback,
    ) {
        let groups = group(statuses);
        let shape_held = {
            let rows = self.rows.borrow();
            rows.len()
                == groups
                    .iter()
                    .map(|group| group.accounts.len())
                    .sum::<usize>()
                && rows
                    .iter()
                    .zip(structure(&groups))
                    .all(|(row, key)| row.provider == key.0 && row.account.as_deref() == key.1)
        };
        if shape_held {
            self.update(&groups, definitions, is_waiting);
        } else {
            self.rebuild(
                &groups,
                definitions,
                is_waiting,
                &on_edit,
                &on_remove,
                &on_add_account,
            );
        }
        self.empty.set_visible(groups.is_empty());
    }

    /// Draws the whole list fresh. Only reached when the shape changed — an account or a
    /// provider appearing or disappearing — because the rows carry no text of their own
    /// to lose, and a list redrawn on every poll would take the click with it.
    fn rebuild(
        &self,
        groups: &[ProviderGroup],
        definitions: &[ProviderDefinition],
        is_waiting: &dyn Fn(&str, &str) -> bool,
        on_edit: &IdentityCallback,
        on_remove: &IdentityCallback,
        on_add_account: &ProviderCallback,
    ) {
        let mut rows = self.rows.borrow_mut();
        for row in std::mem::take(&mut *rows) {
            self.group.remove(&row.row);
        }
        for group in groups {
            let definition = definitions
                .iter()
                .find(|definition| definition.provider == group.provider);
            let built = provider_row(
                definition,
                group,
                is_waiting,
                Rc::clone(on_edit),
                Rc::clone(on_remove),
                Rc::clone(on_add_account),
            );
            self.group.add(&built.row);
            rows.push(built);
            for status in &group.accounts[1..] {
                let built = account_row(
                    definition,
                    status,
                    is_waiting,
                    Rc::clone(on_edit),
                    Rc::clone(on_remove),
                );
                self.group.add(&built.row);
                rows.push(built);
            }
        }
    }

    /// Refreshes the words on rows whose shape did not change.
    fn update(
        &self,
        groups: &[ProviderGroup],
        definitions: &[ProviderDefinition],
        is_waiting: &dyn Fn(&str, &str) -> bool,
    ) {
        let rows = self.rows.borrow();
        for row in rows.iter() {
            let Some(group) = groups.iter().find(|group| group.provider == row.provider) else {
                continue;
            };
            let definition = definitions
                .iter()
                .find(|definition| definition.provider == group.provider);
            match row.account.as_deref() {
                None => update_provider_row(row, definition, group, is_waiting),
                Some(account) => {
                    if let Some(status) = group
                        .accounts
                        .iter()
                        .find(|status| status.account == account)
                    {
                        update_account_row(row, definition, status, is_waiting);
                    }
                }
            }
        }
    }
}

/// The provider's own row: its mark, its name, and the state of its first account — which
/// is the account the pen opens, and the only account a provider with just the one has
/// ever shown. The "+" beside the pen adds another, for providers whose credential a
/// second account can hold its own copy of.
fn provider_row(
    definition: Option<&ProviderDefinition>,
    group: &ProviderGroup,
    is_waiting: &dyn Fn(&str, &str) -> bool,
    on_edit: IdentityCallback,
    on_remove: IdentityCallback,
    on_add_account: ProviderCallback,
) -> ConfiguredRow {
    let status = &group.accounts[0];
    let image = mark::image();
    mark::set(&image, &status.provider);
    let row = adw::ActionRow::new();
    // The picker's stay-off-markup rule again: the title and the subtitle both carry
    // the daemon's own words, which are data, never markup.
    row.set_use_markup(false);
    row.add_prefix(&image);

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add account")
        .valign(gtk::Align::Center)
        .build();
    add.connect_clicked({
        let on_add_account = Rc::clone(&on_add_account);
        let provider = group.provider.clone();
        move |_| on_add_account(provider.clone())
    });

    let edit = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Edit provider")
        .valign(gtk::Align::Center)
        .sensitive(definition.is_some())
        .build();
    edit.connect_clicked({
        let on_edit = Rc::clone(&on_edit);
        let provider = status.provider.clone();
        let account = status.account.clone();
        move |_| on_edit(provider.clone(), account.clone())
    });

    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove provider")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    remove.connect_clicked({
        let on_remove = Rc::clone(&on_remove);
        let provider = status.provider.clone();
        let account = status.account.clone();
        move |_| on_remove(provider.clone(), account.clone())
    });

    // Suffixes stack left to right in the order they are added, which is what puts the
    // "+" left of the pen and the trash outside both.
    row.add_suffix(&add);
    row.add_suffix(&edit);
    row.add_suffix(&remove);

    let built = ConfiguredRow {
        provider: status.provider.clone(),
        account: None,
        row,
        image,
        edit,
        add: Some(add),
    };
    update_provider_row(&built, definition, group, is_waiting);
    built
}

/// One account nested under its provider, indented to say which row it belongs to.
fn account_row(
    definition: Option<&ProviderDefinition>,
    status: &ProviderStatus,
    is_waiting: &dyn Fn(&str, &str) -> bool,
    on_edit: IdentityCallback,
    on_remove: IdentityCallback,
) -> ConfiguredRow {
    let image = mark::image();
    mark::set(&image, &status.provider);
    let row = adw::ActionRow::builder()
        .margin_start(24)
        .use_markup(false)
        .build();
    // A dimmed corner tying the row to the provider above it — the margin alone read as
    // a stray indent rather than as nesting. Pango falls back to a font that draws the
    // glyph when the interface font has none of its own.
    let corner = gtk::Label::builder()
        .label("⌞")
        .valign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .build();
    row.add_prefix(&corner);
    row.add_prefix(&image);

    let edit = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Edit account")
        .valign(gtk::Align::Center)
        .build();
    edit.connect_clicked({
        let on_edit = Rc::clone(&on_edit);
        let provider = status.provider.clone();
        let account = status.account.clone();
        move |_| on_edit(provider.clone(), account.clone())
    });

    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove account")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    remove.connect_clicked({
        let on_remove = Rc::clone(&on_remove);
        let provider = status.provider.clone();
        let account = status.account.clone();
        move |_| on_remove(provider.clone(), account.clone())
    });

    row.add_suffix(&edit);
    row.add_suffix(&remove);

    let built = ConfiguredRow {
        provider: status.provider.clone(),
        account: Some(status.account.clone()),
        row,
        image,
        edit,
        add: None,
    };
    update_account_row(&built, definition, status, is_waiting);
    built
}

fn update_provider_row(
    row: &ConfiguredRow,
    definition: Option<&ProviderDefinition>,
    group: &ProviderGroup,
    is_waiting: &dyn Fn(&str, &str) -> bool,
) {
    let status = &group.accounts[0];
    let title = definition
        .map(|definition| definition.title.clone())
        .unwrap_or_else(|| provider_label(&status.provider));
    row.row.set_title(&title);
    row.row.set_subtitle(&status_text(
        definition,
        status,
        is_waiting(&status.provider, &status.account),
    ));
    // A provider with a source selector to pick at counts as editable however it logs in:
    // which local login to read is configuration, not something polling works out.
    let editable = definition.is_some_and(opens_detail_after_add);
    row.edit.set_visible(editable);
    row.edit.set_sensitive(editable);
    if let Some(add) = &row.add {
        let capable = definition.is_some_and(multi_account_capable);
        add.set_visible(capable);
        add.set_sensitive(capable);
    }
    mark::set(&row.image, &status.provider);
}

fn update_account_row(
    row: &ConfiguredRow,
    definition: Option<&ProviderDefinition>,
    status: &ProviderStatus,
    is_waiting: &dyn Fn(&str, &str) -> bool,
) {
    // The account's own name: the daemon publishes the label, and derives it from the id
    // until a rename says otherwise.
    row.row.set_title(
        status
            .account_label
            .as_deref()
            .unwrap_or(status.account.as_str()),
    );
    row.row.set_subtitle(&status_text(
        definition,
        status,
        is_waiting(&status.provider, &status.account),
    ));
    // The same editability rule the provider's own row applies. For every provider that
    // can hold a second account it is true by construction (its credential is key or
    // oauth), but a missing definition draws no pen here either, exactly as above.
    let editable = definition.is_some_and(opens_detail_after_add);
    row.edit.set_visible(editable);
    row.edit.set_sensitive(editable);
    mark::set(&row.image, &status.provider);
}

/// The one line of state under a row's name.
fn status_text(
    definition: Option<&ProviderDefinition>,
    status: &ProviderStatus,
    waiting: bool,
) -> String {
    if waiting {
        "Waiting for your browser…".into()
    } else if let Some(definition) = definition {
        model::connection_text(definition, status)
    } else {
        status
            .message
            .clone()
            .unwrap_or_else(|| status.state.clone())
    }
}

/// Searchable catalog page pushed from the main provider list.
pub(super) struct Picker {
    page: adw::NavigationPage,
    search: gtk::SearchEntry,
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,
    definitions: RefCell<Vec<ProviderDefinition>>,
    statuses: RefCell<Vec<ProviderStatus>>,
    on_select: Rc<dyn Fn(String)>,
}

impl std::fmt::Debug for Picker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Picker")
            .field("page", &self.page)
            .field("search", &self.search)
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

impl Picker {
    pub(super) fn new(on_select: Rc<dyn Fn(String)>) -> Rc<Self> {
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search providers")
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Search providers")]);
        let group = adw::PreferencesGroup::builder()
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let clamp = adw::Clamp::builder().child(&group).build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        content.append(&search);
        content.append(&scroller);

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new("Add provider", "")));
        let toolbar = adw::ToolbarView::builder().content(&content).build();
        toolbar.add_top_bar(&header);
        let page = adw::NavigationPage::new(&toolbar, "Add provider");

        let picker = Rc::new(Self {
            page,
            search,
            group,
            rows: RefCell::new(Vec::new()),
            definitions: RefCell::new(Vec::new()),
            statuses: RefCell::new(Vec::new()),
            on_select,
        });
        picker.search.connect_search_changed({
            let weak = Rc::downgrade(&picker);
            move |_| {
                if let Some(picker) = weak.upgrade() {
                    picker.rebuild();
                }
            }
        });
        picker
    }

    pub(super) fn page(&self) -> &adw::NavigationPage {
        &self.page
    }

    pub(super) fn apply(&self, definitions: &[ProviderDefinition], statuses: &[ProviderStatus]) {
        *self.definitions.borrow_mut() = definitions.to_vec();
        *self.statuses.borrow_mut() = statuses.to_vec();
        self.rebuild();
    }

    fn rebuild(&self) {
        let matches: Vec<ProviderDefinition> = {
            let definitions = self.definitions.borrow();
            let statuses = self.statuses.borrow();
            model::addable(&definitions, &statuses, &self.search.text())
                .into_iter()
                .cloned()
                .collect()
        };

        let mut rows = self.rows.borrow_mut();
        for row in std::mem::take(&mut *rows) {
            self.group.remove(&row);
        }
        for definition in matches {
            let image = mark::image();
            mark::set(&image, &definition.provider);
            // Markup off: a title here is a name — `ai&` among them — and a preferences
            // row parses its title as Pango markup by default, where a stray `&` fails
            // the parse and leaves the row untitled.
            let row = adw::ActionRow::builder()
                .title(&definition.title)
                .subtitle(&definition.provider)
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_prefix(&image);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated({
                let on_select = Rc::clone(&self.on_select);
                let provider = definition.provider.clone();
                move |_| on_select(provider.clone())
            });
            self.group.add(&row);
            rows.push(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, ProviderId, ProviderStatus};

    use super::{ProviderGroup, group, structure};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account))
    }

    fn keys(groups: &[ProviderGroup]) -> Vec<(&str, Vec<&str>)> {
        groups
            .iter()
            .map(|group| {
                (
                    group.provider.as_str(),
                    group
                        .accounts
                        .iter()
                        .map(|status| status.account.as_str())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn statuses_of_one_provider_are_gathered_under_it_in_first_seen_order() {
        // A status for a second account can arrive after another provider's, from a
        // locally added pending row as much as from the daemon; the provider it belongs
        // to is where it lands.
        let groups = group(&[
            status("kimi", "default"),
            status("zai", "default"),
            status("kimi", "work"),
        ]);

        assert_eq!(
            keys(&groups),
            vec![("kimi", vec!["default", "work"]), ("zai", vec!["default"])]
        );
    }

    #[test]
    fn accounts_keep_the_order_they_arrived_in() {
        let groups = group(&[
            status("kimi", "team"),
            status("kimi", "default"),
            status("kimi", "work"),
        ]);

        assert_eq!(
            keys(&groups),
            vec![("kimi", vec!["team", "default", "work"])]
        );
    }

    #[test]
    fn a_group_draws_one_provider_key_then_one_key_per_account_beyond_the_first() {
        // The shape key apply() diffs held rows against: None is the provider's own row,
        // and each account after the first is Some(...) in configured order. A provider
        // with one account contributes its single None key and nothing else.
        let groups = group(&[
            status("kimi", "default"),
            status("kimi", "work"),
            status("kimi", "team"),
            status("zai", "default"),
        ]);

        assert_eq!(
            structure(&groups),
            [
                ("kimi", None),
                ("kimi", Some("work")),
                ("kimi", Some("team")),
                ("zai", None),
            ]
        );
    }

    #[test]
    fn no_statuses_means_no_rows() {
        assert!(group(&[]).is_empty());
    }
}
