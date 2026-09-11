//! Markdown, as far as a release-notes dialog needs it.
//!
//! Release notes are written by whoever published the release and arrive over the bus
//! verbatim. They are neither trusted markup nor a document worth a full renderer: what
//! this produces is a flat list of [`Block`]s, each carrying Pango markup for one label.
//! Parsing is CommonMark by way of `pulldown-cmark`, so emphasis, links and code spans are
//! decided by a parser rather than by regular expressions that would disagree with GitHub's
//! own rendering of the same text.
//!
//! Two deliberate departures from a browser:
//!
//! * **Raw HTML is dropped, never shown.** Release bodies carry `<!-- -->` comments and the
//!   occasional `<details>`; Pango would reject the tags and printing them verbatim is
//!   noise. Their text content still comes through as text.
//! * **Bare URLs become links.** GitHub's generated notes are mostly bare pull-request
//!   URLs, which CommonMark leaves as plain text. A preview of those notes where the links
//!   are dead would send every reader to the browser this dialog exists to postpone.
//!
//! Everything that reaches a label is escaped here. A release body containing `<b>` or a
//! stray `&` is text, and Pango failing to parse a label's markup would drop the label's
//! contents entirely.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// One block of a rendered document: one widget's worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    /// A heading and its depth, 1 to 6.
    Heading { level: u8, markup: String },
    /// A run of prose.
    Paragraph { markup: String },
    /// One list item, with the marker its list gives it and how deeply it is nested.
    Item {
        depth: usize,
        marker: String,
        markup: String,
    },
    /// A fenced or indented code block, verbatim and unescaped.
    Code { text: String },
    /// A thematic break.
    Rule,
}

/// The blocks of one Markdown document, in reading order.
pub(crate) fn blocks(markdown: &str) -> Vec<Block> {
    Render::default().run(markdown)
}

#[derive(Debug, Default)]
struct Render {
    blocks: Vec<Block>,
    /// The inline run being accumulated: Pango markup, already escaped.
    markup: String,
    /// Plain text seen since the last markup was written, not yet escaped into `markup`.
    ///
    /// A parser splits text at every position emphasis *could* have started, so
    /// `example.com/a_b` arrives in three pieces. Bare URLs are recognised across the whole
    /// run rather than per piece, which is what this buffer is for.
    pending: String,
    /// One entry per open list. `Some(n)` is an ordered list's next number.
    lists: Vec<Option<u64>>,
    /// One entry per open list item: its marker, and whether it has already been pushed
    /// (which happens when a nested list interrupts it).
    items: Vec<(String, bool)>,
    /// The heading being accumulated, if any.
    heading: Option<u8>,
    /// The code block being accumulated, if any.
    code: Option<String>,
    /// How many links are open, so text inside one is never linkified again.
    links: usize,
}

impl Render {
    fn run(mut self, markdown: &str) -> Vec<Block> {
        // Strikethrough and task lists are GitHub's, are used in release notes, and cost
        // nothing to accept. Tables are not enabled: a table rendered as one label per row
        // of pipes is no worse than the source, and a dialog is not where a layout engine
        // earns its keep.
        let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for event in Parser::new_ext(markdown, options) {
            self.event(event);
        }
        self.blocks
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some(code) => code.push_str(&text),
                None => self.pending.push_str(&text),
            },
            Event::Code(code) => {
                self.flush_text();
                self.markup.push_str("<tt>");
                escape(&code, &mut self.markup);
                self.markup.push_str("</tt>");
            }
            // A line break inside a paragraph is a space, the way every Markdown renderer
            // reflows it: the label decides where the line ends, from the width it is given.
            // Both go through the text buffer, because a URL ends at whitespace either way.
            Event::SoftBreak => self.pending.push(' '),
            Event::HardBreak => self.pending.push('\n'),
            Event::TaskListMarker(done) => {
                self.flush_text();
                self.markup.push_str(if done { "☑ " } else { "☐ " });
            }
            Event::Rule => self.blocks.push(Block::Rule),
            // Raw HTML, footnote references and maths: dropped rather than shown. See the
            // module documentation.
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_) => {}
        }
    }

    /// Escapes the buffered text into the markup run, linkifying bare URLs outside links.
    fn flush_text(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        if self.links > 0 {
            escape(&pending, &mut self.markup);
        } else {
            linkify(&pending, &mut self.markup);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        // Text before this tag belongs to the run that ends here, and a URL never spans a
        // tag boundary, so the buffer is settled first.
        self.flush_text();
        match tag {
            Tag::Heading { level, .. } => {
                self.markup.clear();
                self.heading = Some(depth(level));
            }
            // A loose list item's text arrives wrapped in a paragraph. Inside an item that
            // is a continuation line, not a new block.
            Tag::Paragraph if self.in_item() => {
                if !self.markup.is_empty() {
                    self.markup.push('\n');
                }
            }
            Tag::Paragraph => self.markup.clear(),
            Tag::List(start) => {
                // A nested list interrupts its parent item: whatever that item said before
                // the nesting is its own line, and the parent's own end must not repeat it.
                self.flush_item();
                self.lists.push(start);
            }
            Tag::Item => {
                self.markup.clear();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}.");
                        *number += 1;
                        marker
                    }
                    _ => "•".to_owned(),
                };
                self.items.push((marker, false));
            }
            Tag::CodeBlock(_) => self.code = Some(String::new()),
            Tag::Emphasis => self.markup.push_str("<i>"),
            Tag::Strong => self.markup.push_str("<b>"),
            Tag::Strikethrough => self.markup.push_str("<s>"),
            Tag::Link { dest_url, .. } => {
                self.links += 1;
                self.markup.push_str("<a href=\"");
                escape(&dest_url, &mut self.markup);
                self.markup.push_str("\">");
            }
            // An image cannot be fetched by a dialog that does no I/O, so only its
            // alternative text — which arrives as the tag's own contents — is kept.
            // Block quotes, footnote definitions and table parts contribute their text to
            // the blocks they contain.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        self.flush_text();
        match tag {
            TagEnd::Heading(_) => {
                if let Some(level) = self.heading.take() {
                    let markup = self.take();
                    self.push(Block::Heading { level, markup });
                }
            }
            TagEnd::Paragraph if self.in_item() => {}
            TagEnd::Paragraph => {
                let markup = self.take();
                self.push(Block::Paragraph { markup });
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_item();
                self.items.pop();
            }
            TagEnd::CodeBlock => {
                if let Some(text) = self.code.take() {
                    self.push(Block::Code {
                        text: text.trim_end().to_owned(),
                    });
                }
            }
            TagEnd::Emphasis => self.markup.push_str("</i>"),
            TagEnd::Strong => self.markup.push_str("</b>"),
            TagEnd::Strikethrough => self.markup.push_str("</s>"),
            TagEnd::Link => {
                self.links = self.links.saturating_sub(1);
                self.markup.push_str("</a>");
            }
            _ => {}
        }
    }

    fn in_item(&self) -> bool {
        self.items.last().is_some_and(|(_, pushed)| !pushed)
    }

    /// Emits the item being accumulated, if it has anything to say.
    fn flush_item(&mut self) {
        let depth = self.lists.len().saturating_sub(1);
        let Some((marker, pushed)) = self.items.last_mut() else {
            return;
        };
        if *pushed || self.markup.trim().is_empty() {
            return;
        }
        let marker = marker.clone();
        *pushed = true;
        let markup = self.take();
        self.push(Block::Item {
            depth,
            marker,
            markup,
        });
    }

    fn take(&mut self) -> String {
        self.flush_text();
        std::mem::take(&mut self.markup).trim().to_owned()
    }

    /// Keeps empty blocks out: a stray blank paragraph is vertical space nobody asked for.
    fn push(&mut self, block: Block) {
        let empty = match &block {
            Block::Heading { markup, .. } | Block::Paragraph { markup } => markup.is_empty(),
            Block::Item { markup, .. } => markup.is_empty(),
            Block::Code { text } => text.is_empty(),
            Block::Rule => false,
        };
        if !empty {
            self.blocks.push(block);
        }
    }
}

fn depth(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Appends `text` as Pango markup, escaping everything markup would otherwise read.
///
/// `"` is escaped too, because the same function writes link destinations into an
/// attribute, where a quotation mark would end it. An apostrophe is not: attributes here
/// are double-quoted, and "What's Changed" is a heading, not an entity.
fn escape(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
}

/// Appends `text`, turning bare `http(s)` URLs into links.
fn linkify(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(start) = url_start(rest) {
        escape(&rest[..start], out);
        let (url, tail) = split_url(&rest[start..]);
        out.push_str("<a href=\"");
        escape(url, out);
        out.push_str("\">");
        escape(url, out);
        out.push_str("</a>");
        rest = tail;
    }
    escape(rest, out);
}

/// Where the next bare URL begins, if any. Only at a word boundary: `nothttps://x` is a
/// word, not a link.
fn url_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = text[from..].find("http") {
        let at = from + offset;
        let boundary = text[..at]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_alphanumeric());
        let scheme = text[at..].starts_with("https://") || text[at..].starts_with("http://");
        if boundary && scheme {
            return Some(at);
        }
        from = at + "http".len();
    }
    None
}

/// Splits a bare URL from the text after it.
///
/// A URL ends at whitespace, and trailing sentence punctuation belongs to the sentence
/// rather than to the link. A closing bracket is only kept when the URL opened one, which
/// is what keeps `(see https://example.com/a)` from linking the parenthesis.
fn split_url(text: &str) -> (&str, &str) {
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let mut url = &text[..end];
    while let Some(last) = url.chars().next_back() {
        let trailing = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '_' => true,
            ')' => url.matches('(').count() < url.matches(')').count(),
            ']' => url.matches('[').count() < url.matches(']').count(),
            _ => false,
        };
        if !trailing {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    (url, &text[url.len()..])
}

#[cfg(test)]
mod tests {
    use super::{Block, blocks};

    fn markup(block: &Block) -> &str {
        match block {
            Block::Heading { markup, .. }
            | Block::Paragraph { markup }
            | Block::Item { markup, .. } => markup,
            Block::Code { text } => text,
            Block::Rule => "",
        }
    }

    #[test]
    fn the_shape_github_generates_survives_as_headings_and_items() {
        let rendered = blocks(concat!(
            "## What's Changed\n",
            "* fix(ui): a fix by @zbndev in https://github.com/zbndev/tidemark/pull/69\n",
            "\n",
            "**Full Changelog**: https://github.com/zbndev/tidemark/compare/v0.1.0...v0.2.0\n",
        ));

        assert_eq!(
            rendered[0],
            Block::Heading {
                level: 2,
                markup: "What's Changed".into(),
            }
        );
        assert_eq!(
            rendered[1],
            Block::Item {
                depth: 0,
                marker: "•".into(),
                markup: concat!(
                    "fix(ui): a fix by @zbndev in ",
                    "<a href=\"https://github.com/zbndev/tidemark/pull/69\">",
                    "https://github.com/zbndev/tidemark/pull/69</a>",
                )
                .into(),
            },
            "a bare pull-request URL is a link, or the whole preview is dead text"
        );
        assert!(
            markup(&rendered[2]).starts_with("<b>Full Changelog</b>: <a href=\""),
            "got {:?}",
            rendered[2]
        );
    }

    #[test]
    fn emphasis_code_and_links_become_pango_markup() {
        let rendered = blocks("*Cards* keep `MIN_WIDTH`, see [the PR](https://example.com/a).");

        assert_eq!(
            markup(&rendered[0]),
            concat!(
                "<i>Cards</i> keep <tt>MIN_WIDTH</tt>, see ",
                "<a href=\"https://example.com/a\">the PR</a>.",
            )
        );
    }

    #[test]
    fn raw_html_is_dropped_and_everything_else_is_escaped() {
        let rendered = blocks("Use <b>&amp;</b> in \"quotes\" <!-- and a comment -->\n");

        assert_eq!(
            markup(&rendered[0]),
            "Use &amp; in &quot;quotes&quot;",
            "tags and comments go, their text stays, and what reaches Pango is escaped: a \
             label whose markup does not parse loses all of its contents"
        );
    }

    #[test]
    fn ordered_and_nested_lists_keep_their_markers_and_depth() {
        let rendered = blocks(concat!(
            "1. first\n",
            "2. second\n",
            "   - nested\n",
            "\n",
            "```\ncargo test\n```\n",
            "\n---\n",
        ));

        assert_eq!(
            rendered,
            vec![
                Block::Item {
                    depth: 0,
                    marker: "1.".into(),
                    markup: "first".into(),
                },
                Block::Item {
                    depth: 0,
                    marker: "2.".into(),
                    markup: "second".into(),
                },
                Block::Item {
                    depth: 1,
                    marker: "•".into(),
                    markup: "nested".into(),
                },
                Block::Code {
                    text: "cargo test".into(),
                },
                Block::Rule,
            ]
        );
    }

    #[test]
    fn a_url_keeps_its_own_brackets_and_drops_the_sentences() {
        let rendered = blocks("See https://example.com/a_(b), then https://example.com/c.\n");

        assert_eq!(
            markup(&rendered[0]),
            concat!(
                "See <a href=\"https://example.com/a_(b)\">https://example.com/a_(b)</a>, ",
                "then <a href=\"https://example.com/c\">https://example.com/c</a>.",
            )
        );
    }

    #[test]
    fn a_word_ending_in_a_scheme_is_not_a_link() {
        let rendered = blocks("nothttps://example.com is not a link\n");

        assert_eq!(markup(&rendered[0]), "nothttps://example.com is not a link");
    }

    #[test]
    fn empty_notes_render_nothing_rather_than_a_blank_block() {
        assert!(blocks("").is_empty());
        assert!(blocks("\n\n   \n").is_empty());
    }
}
