use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use tidemark_types::{ProviderDefinition, ProviderStatus, provider_label};

use super::{model, opens_detail_after_add};
use crate::mark;

type IdentityCallback = Rc<dyn Fn(String, String)>;

/// The configured rows on the dialog's main page.
#[derive(Debug)]
pub(super) struct ConfiguredList {
    pub(super) group: adw::PreferencesGroup,
    empty: adw::StatusPage,
    rows: RefCell<Vec<ConfiguredRow>>,
}

#[derive(Debug)]
struct ConfiguredRow {
    provider: String,
    account: String,
    row: adw::ActionRow,
    image: gtk::Image,
    edit: gtk::Button,
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
    ) {
        let mut rows = self.rows.borrow_mut();
        let (kept, removed): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *rows).into_iter().partition(|row| {
                statuses
                    .iter()
                    .any(|status| status.provider == row.provider && status.account == row.account)
            });
        *rows = kept;
        for removed in removed {
            self.group.remove(&removed.row);
        }

        for status in statuses {
            let definition = definitions
                .iter()
                .find(|definition| definition.provider == status.provider);
            if let Some(existing) = rows
                .iter()
                .find(|row| row.provider == status.provider && row.account == status.account)
            {
                update_row(existing, definition, status, is_waiting);
                continue;
            }

            let built = configured_row(
                definition,
                status,
                is_waiting,
                Rc::clone(&on_edit),
                Rc::clone(&on_remove),
            );
            self.group.add(&built.row);
            rows.push(built);
        }

        self.empty.set_visible(rows.is_empty());
    }
}

fn configured_row(
    definition: Option<&ProviderDefinition>,
    status: &ProviderStatus,
    is_waiting: &dyn Fn(&str, &str) -> bool,
    on_edit: IdentityCallback,
    on_remove: IdentityCallback,
) -> ConfiguredRow {
    let image = mark::image();
    mark::set(&image, &status.provider);
    let row = adw::ActionRow::new();
    // The picker's stay-off-markup rule again: the title and the subtitle both carry
    // the daemon's own words, which are data, never markup.
    row.set_use_markup(false);
    row.add_prefix(&image);

    let edit = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Edit provider")
        .valign(gtk::Align::Center)
        .sensitive(definition.is_some())
        .build();
    edit.connect_clicked({
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
        let provider = status.provider.clone();
        let account = status.account.clone();
        move |_| on_remove(provider.clone(), account.clone())
    });

    row.add_suffix(&edit);
    row.add_suffix(&remove);

    let built = ConfiguredRow {
        provider: status.provider.clone(),
        account: status.account.clone(),
        row,
        image,
        edit,
    };
    update_row(&built, definition, status, is_waiting);
    built
}

fn update_row(
    row: &ConfiguredRow,
    definition: Option<&ProviderDefinition>,
    status: &ProviderStatus,
    is_waiting: &dyn Fn(&str, &str) -> bool,
) {
    let title = definition
        .map(|definition| definition.title.clone())
        .unwrap_or_else(|| provider_label(&status.provider));
    row.row.set_title(&title);
    let subtitle = if is_waiting(&status.provider, &status.account) {
        "Waiting for your browser…".into()
    } else if let Some(definition) = definition {
        model::connection_text(definition, status)
    } else {
        status
            .message
            .clone()
            .unwrap_or_else(|| status.state.clone())
    };
    row.row.set_subtitle(&subtitle);
    // A provider with a source selector to pick at counts as editable however it logs in:
    // which local login to read is configuration, not something polling works out.
    let editable = definition.is_some_and(opens_detail_after_add);
    row.edit.set_visible(editable);
    row.edit.set_sensitive(editable);
    mark::set(&row.image, &status.provider);
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
