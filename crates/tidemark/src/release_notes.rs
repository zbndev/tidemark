//! The changelog preview behind the header's update button.
//!
//! The daemon found a newer release and published its notes; this shows them before anyone
//! leaves for a browser. The dialog is deliberately a preview and not an updater: Tidemark
//! is installed by a package manager, an installer or a distribution, and the button that
//! ends the dialog goes to the release page rather than pretending this process can replace
//! itself.
//!
//! Every label here is Pango markup produced by [`crate::markdown`], which escapes the
//! release body before adding any markup of its own.

use adw::prelude::*;
use gtk::glib;

use crate::markdown::{self, Block};
use crate::update;

/// How wide and tall the dialog opens. A changelog is read in lines, not in paragraphs of
/// prose, so it is given a comfortable measure rather than the width of its longest URL.
const WIDTH: i32 = 560;
const HEIGHT: i32 = 620;
/// How far one level of list nesting is indented.
const INDENT: i32 = 18;

/// Shows `notes` for `version` over `parent`.
///
/// `notes` is Markdown as its publisher wrote it. An empty body is not this dialog's
/// problem to present: the caller opens the release page instead, because a dialog whose
/// only content is "no notes" is a click that told the reader nothing.
pub(crate) fn present(parent: &impl IsA<gtk::Widget>, version: &str, notes: &str) {
    let dialog = adw::Dialog::builder()
        .title(format!("Tidemark {version}"))
        .content_width(WIDTH)
        .content_height(HEIGHT)
        .build();

    let scroll = gtk::ScrolledWindow::builder()
        // Never horizontally: the notes wrap to the dialog's width, including inside the
        // long URLs GitHub's generated notes are made of.
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&body(notes))
        .build();

    // The container is the box around the scroll, not the scroll itself: it draws the
    // recessed surface and the rounded corners, while the scroll clips its own square
    // viewport inside it. Rounding the scroll instead needs `Overflow::Hidden`, and that
    // clip lands on the text — the first line loses its top pixels to it. A box rather
    // than a `GtkFrame`, whose separate `border` node would draw the theme's border
    // beneath ours.
    let framed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["release-notes"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .build();
    framed.append(&scroll);

    let cancel = gtk::Button::with_label("Cancel");
    let download = gtk::Button::builder()
        .label("Download on GitHub")
        .css_classes(["suggested-action"])
        .build();
    let actions = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_start(18)
        .margin_end(18)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    actions.append(&cancel);
    actions.append(&download);

    let toolbar = adw::ToolbarView::builder().content(&framed).build();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.add_bottom_bar(&actions);
    dialog.set_child(Some(&toolbar));

    let closing = dialog.clone();
    cancel.connect_clicked(move |_| {
        closing.close();
    });
    let url = update::release_url(version);
    let closing = dialog.clone();
    download.connect_clicked(move |button| {
        // The window under the dialog, not the dialog: it is about to close, and a launcher
        // parented to a widget on its way out has nothing to be modal to.
        let window = button.root().and_downcast::<gtk::Window>();
        let url = url.clone();
        closing.close();
        glib::spawn_future_local(async move {
            let launcher = gtk::UriLauncher::new(&url);
            if let Err(error) = launcher.launch_future(window.as_ref()).await {
                tracing::warn!(%error, "could not open the Tidemark release page");
            }
        });
    });

    // Focus starts on a button, not in the notes. GTK selects the whole of a selectable
    // label the moment it takes focus, so the dialog would open with its first line
    // highlighted; Cancel rather than Download, because the key that then works by itself
    // must not be the one that opens a browser.
    dialog.set_focus(Some(&cancel));
    dialog.present(Some(parent));
}

/// The notes as widgets: one per block, in reading order.
fn body(notes: &str) -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    for block in markdown::blocks(notes) {
        content.append(&widget(&block));
    }
    content
}

/// One block's widget.
fn widget(block: &Block) -> gtk::Widget {
    match block {
        Block::Heading { level, markup } => {
            let heading = label(markup);
            heading.add_css_class(heading_class(*level));
            // A heading belongs to what follows it, so the air goes above.
            heading.set_margin_top(8);
            heading.upcast()
        }
        Block::Paragraph { markup } => label(markup).upcast(),
        Block::Item {
            depth,
            marker,
            markup,
        } => {
            let row = gtk::Box::builder()
                .spacing(8)
                .margin_start(4 + INDENT * i32::try_from(*depth).unwrap_or(0))
                .build();
            let bullet = gtk::Label::builder()
                .label(marker)
                .valign(gtk::Align::Start)
                .css_classes(["dim-label"])
                .build();
            let text = label(markup);
            text.set_hexpand(true);
            row.append(&bullet);
            row.append(&text);
            row.upcast()
        }
        // Code is the one thing here that is not markup: it is shown exactly as written,
        // in a monospaced face, on its own surface.
        Block::Code { text } => {
            let code = gtk::Label::builder()
                .label(text)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .margin_start(10)
                .margin_end(10)
                .margin_top(8)
                .margin_bottom(8)
                .css_classes(["monospace"])
                .build();
            gtk::Frame::builder()
                .child(&code)
                .css_classes(["card"])
                .build()
                .upcast()
        }
        Block::Rule => gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(4)
            .margin_bottom(4)
            .build()
            .upcast(),
    }
}

/// A label carrying Pango markup, wrapping to whatever width it is given.
///
/// `WordChar` rather than `Word`: a pull-request URL is one word, and a label that cannot
/// break inside it would set the dialog's width to the length of the longest link.
/// Selectable, because a changelog is something people copy lines out of.
fn label(markup: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(markup)
        .use_markup(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .halign(gtk::Align::Fill)
        .valign(gtk::Align::Start)
        .selectable(true)
        .build()
}

/// The libadwaita style for a heading of this depth.
///
/// The dialog's own title already carries the version, so a release body opening with `#`
/// is a section heading here rather than the page's title: the scale starts one step down
/// from where a document would start it, and everything past the third level shares the
/// smallest style there is.
fn heading_class(level: u8) -> &'static str {
    match level {
        1 => "title-2",
        2 => "title-3",
        3 => "title-4",
        _ => "heading",
    }
}

#[cfg(test)]
mod tests {
    use super::heading_class;

    #[test]
    fn headings_start_one_step_below_a_documents_title_and_bottom_out() {
        // The dialog's title bar already says which release this is.
        assert_eq!(heading_class(1), "title-2");
        assert_eq!(heading_class(2), "title-3");
        assert_eq!(heading_class(6), "heading");
    }

    #[test]
    fn the_notes_sit_on_a_surface_darker_than_the_dialog_in_either_theme() {
        // The container asks for `release-notes`, and what makes it a container is the
        // shade: a stylesheet that stopped defining it, or that reached for libadwaita's
        // `view` — white in a light theme, and so invisible against the dialog — would
        // leave a plain block of text and nothing would fail.
        assert!(
            crate::style::STYLE
                .contains(".release-notes {\n    background-color: shade(@window_bg_color,")
        );
    }
}
