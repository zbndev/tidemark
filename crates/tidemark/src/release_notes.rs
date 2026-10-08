//! The changelog preview behind the header's update button.
//!
//! The daemon found a newer release and published its notes; this shows them before anyone
//! leaves for a browser. The dialog is deliberately a preview and not an updater: Tidemark
//! is installed by a package manager, an installer or a distribution, and the button that
//! ends the dialog goes to the release page rather than pretending this process can replace
//! itself.
//!
//! Every block's text is styled-text Markdown produced by [`crate::markdown`], which
//! escapes the release body before adding any markup of its own.

use slint::{ComponentHandle, ModelRc, StyledText, VecModel};

use crate::markdown::{self, Block};
use crate::{AppWindow, NoteBlock, ReleaseNotes, update};

/// Wires the dialog's buttons and links, once, for the life of the window.
pub fn install(ui: &AppWindow) {
    let notes = ui.global::<ReleaseNotes>();
    let weak = ui.as_weak();
    notes.on_close(move || close(&weak));
    let weak = ui.as_weak();
    notes.on_download(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let version = ui.global::<ReleaseNotes>().get_version();
        close(&weak);
        open(&update::release_url(&version));
    });
    notes.on_open_link(|url| {
        // A link the publisher wrote: only to the web. A relative one has no page to be
        // relative to, and anything else is not this dialog's to launch.
        if url.starts_with("https://") || url.starts_with("http://") {
            open(&url);
        }
    });
}

/// Shows `notes` for `version`.
///
/// `notes` is Markdown as its publisher wrote it. An empty body is not this dialog's
/// problem to present: the caller opens the release page instead, because a dialog whose
/// only content is "no notes" is a click that told the reader nothing.
pub fn present(ui: &AppWindow, version: &str, notes: &str) {
    let blocks: Vec<NoteBlock> = markdown::blocks(notes).iter().map(note).collect();
    let dialog = ui.global::<ReleaseNotes>();
    dialog.set_version(version.into());
    dialog.set_blocks(ModelRc::new(VecModel::from(blocks)));
    dialog.set_open(true);
}

fn close(ui: &slint::Weak<AppWindow>) {
    if let Some(ui) = ui.upgrade() {
        ui.global::<ReleaseNotes>().set_open(false);
    }
}

fn open(url: &str) {
    if let Err(error) = webbrowser::open(url) {
        tracing::warn!(%error, url, "could not open a link from the release notes");
    }
}

/// One block as the markup draws it.
fn note(block: &Block) -> NoteBlock {
    let (kind, level, depth, marker, text) = match block {
        Block::Heading { level, markdown } => (0, *level, 0, "", styled(markdown)),
        Block::Paragraph { markdown } => (1, 0, 0, "", styled(markdown)),
        Block::Item {
            depth,
            marker,
            markdown,
        } => (2, 0, *depth, marker.as_str(), styled(markdown)),
        Block::Code { text } => (3, 0, 0, "", code(text)),
        Block::Rule => (4, 0, 0, "", StyledText::default()),
    };
    NoteBlock {
        kind,
        level: level.into(),
        depth: i32::try_from(depth).unwrap_or(i32::MAX),
        marker: marker.into(),
        text,
    }
}

/// A block's Markdown as Slint draws it. Everything in it was escaped or written by
/// [`crate::markdown`], so a refusal is a bug there; the text is still worth showing,
/// unstyled, rather than a gap in the notes.
fn styled(markdown: &str) -> StyledText {
    StyledText::from_markdown(markdown).unwrap_or_else(|error| {
        tracing::warn!(%error, "release notes block shown as plain text");
        StyledText::from_plain_text(markdown)
    })
}

/// A code block: verbatim either way, monospaced when Slint accepts its lines as spans.
fn code(text: &str) -> StyledText {
    StyledText::from_markdown(&markdown::code_lines(text))
        .unwrap_or_else(|_| StyledText::from_plain_text(text))
}
