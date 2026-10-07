//! Markdown, as far as a release-notes dialog needs it.
//!
//! Release notes are written by whoever published the release and arrive over the bus
//! verbatim. They are neither trusted markup nor a document worth a full renderer: what
//! this produces is a flat list of [`Block`]s, one element each. Parsing is CommonMark by
//! way of `pulldown-cmark`, so emphasis, links and code spans are decided by a parser
//! rather than by regular expressions that would disagree with GitHub's own rendering.
//!
//! Each block's text is Markdown again, but only the inline subset Slint's `StyledText`
//! draws: emphasis, strong, strikethrough, code spans, links and hard breaks. Slint refuses
//! the whole string over anything else — a heading, a rule, a stray tag — so the blocks are
//! split out here, and everything that reaches it is escaped.
//!
//! Two deliberate departures from a browser:
//!
//! * **Raw HTML is dropped, never shown.** Release bodies carry `<!-- -->` comments and the
//!   occasional `<details>`; printing them verbatim is noise. Their text content still
//!   comes through as text.
//! * **Bare URLs become links.** GitHub's generated notes are mostly bare pull-request
//!   URLs, which CommonMark leaves as plain text. A preview of those notes where the links
//!   are dead would send every reader to the browser this dialog exists to postpone.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// One block of a rendered document: one element's worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    /// A heading and its depth, 1 to 6.
    Heading { level: u8, markdown: String },
    /// A run of prose.
    Paragraph { markdown: String },
    /// One list item, with the marker its list gives it and how deeply it is nested.
    Item {
        depth: usize,
        marker: String,
        markdown: String,
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

/// A code block as styled-text Markdown: one code span per line, since `StyledText` has
/// no code blocks and a span is the only way it sets text in a monospaced face. A blank
/// line is a no-break space, which keeps its height without drawing an empty span.
pub(crate) fn code_lines(text: &str) -> String {
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push_str("\\\n");
        }
        if line.trim().is_empty() {
            out.push('\u{a0}');
        } else {
            code_span(line, &mut out);
        }
    }
    out
}

#[derive(Debug, Default)]
struct Render {
    blocks: Vec<Block>,
    /// The inline run being accumulated: styled-text Markdown, already escaped. A `\n` in
    /// it is a hard break, written out as one by [`Render::take`].
    markdown: String,
    /// Plain text seen since the last markup was written, not yet escaped into `markdown`.
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
    /// The destinations of the links still open, innermost last. Text inside one is never
    /// linkified again.
    links: Vec<String>,
}

impl Render {
    fn run(mut self, markdown: &str) -> Vec<Block> {
        // Strikethrough and task lists are GitHub's, are used in release notes, and cost
        // nothing to accept. Tables are not enabled: a table rendered as one line per row
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
                code_span(&code, &mut self.markdown);
            }
            // A line break inside a paragraph is a space, the way every Markdown renderer
            // reflows it: the text decides where the line ends, from the width it is given.
            // Both go through the text buffer, because a URL ends at whitespace either way.
            Event::SoftBreak => self.pending.push(' '),
            Event::HardBreak => self.pending.push('\n'),
            Event::TaskListMarker(done) => {
                self.flush_text();
                self.markdown.push_str(if done { "☑ " } else { "☐ " });
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

    /// Escapes the buffered text into the run, linkifying bare URLs outside links.
    fn flush_text(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        if !self.links.is_empty() {
            escape(&pending, &mut self.markdown);
        } else {
            linkify(&pending, &mut self.markdown);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        // Text before this tag belongs to the run that ends here, and a URL never spans a
        // tag boundary, so the buffer is settled first.
        self.flush_text();
        match tag {
            Tag::Heading { level, .. } => {
                self.markdown.clear();
                self.heading = Some(depth(level));
            }
            // A loose list item's text arrives wrapped in a paragraph. Inside an item that
            // is a continuation line, not a new block.
            Tag::Paragraph if self.in_item() => {
                if !self.markdown.is_empty() {
                    self.markdown.push('\n');
                }
            }
            Tag::Paragraph => self.markdown.clear(),
            Tag::List(start) => {
                // A nested list interrupts its parent item: whatever that item said before
                // the nesting is its own line, and the parent's own end must not repeat it.
                self.flush_item();
                self.lists.push(start);
            }
            Tag::Item => {
                self.markdown.clear();
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
            Tag::Emphasis => self.markdown.push('*'),
            // A heading is set in bold as a whole, and strong inside bold is nothing to
            // draw — but `****` would be, as literal asterisks.
            Tag::Strong if self.heading.is_none() => self.markdown.push_str("**"),
            Tag::Strikethrough => self.markdown.push_str("~~"),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                self.markdown.push('[');
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
                    let markdown = self.take();
                    let markdown = if markdown.is_empty() {
                        markdown
                    } else {
                        format!("**{markdown}**")
                    };
                    self.push(Block::Heading { level, markdown });
                }
            }
            TagEnd::Paragraph if self.in_item() => {}
            TagEnd::Paragraph => {
                let markdown = self.take();
                self.push(Block::Paragraph { markdown });
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
            TagEnd::Emphasis => self.markdown.push('*'),
            TagEnd::Strong if self.heading.is_none() => self.markdown.push_str("**"),
            TagEnd::Strikethrough => self.markdown.push_str("~~"),
            TagEnd::Link => {
                if let Some(url) = self.links.pop() {
                    close_link(&url, &mut self.markdown);
                }
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
        if *pushed || self.markdown.trim().is_empty() {
            return;
        }
        let marker = marker.clone();
        *pushed = true;
        let markdown = self.take();
        self.push(Block::Item {
            depth,
            marker,
            markdown,
        });
    }

    /// The finished run. Each line is trimmed, since CommonMark would otherwise read four
    /// leading spaces as code; empty lines go, and the rest are joined by hard breaks.
    fn take(&mut self) -> String {
        self.flush_text();
        let run = std::mem::take(&mut self.markdown);
        run.split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\\\n")
    }

    /// Keeps empty blocks out: a stray blank paragraph is vertical space nobody asked for.
    fn push(&mut self, block: Block) {
        let empty = match &block {
            Block::Heading { markdown, .. }
            | Block::Paragraph { markdown }
            | Block::Item { markdown, .. } => markdown.is_empty(),
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

/// Appends `text` as Markdown that reads back as exactly this text.
///
/// Every ASCII punctuation character is backslash-escaped, which CommonMark allows for all
/// of them: no `*`, `[`, `<` or leading `#` in a release body can open anything. A `\n` is
/// passed through as the run's hard-break marker.
fn escape(text: &str, out: &mut String) {
    for character in text.chars() {
        if character.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(character);
    }
}

/// Appends a code span holding `code` verbatim. The fence is one backtick longer than any
/// run inside it, and the padding space on either side is the one CommonMark strips.
fn code_span(code: &str, out: &mut String) {
    let mut longest = 0;
    let mut run = 0;
    for character in code.chars() {
        run = if character == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    out.push_str(&fence);
    out.push(' ');
    out.push_str(code);
    out.push(' ');
    out.push_str(&fence);
}

/// Closes the link whose text has just been written, pointing it at `url`.
fn close_link(url: &str, out: &mut String) {
    out.push_str("](<");
    for character in url.chars() {
        if matches!(character, '<' | '>' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out.push_str(">)");
}

/// Appends `text`, turning bare `http(s)` URLs into links.
fn linkify(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(start) = url_start(rest) {
        escape(&rest[..start], out);
        let (url, tail) = split_url(&rest[start..]);
        out.push('[');
        escape(url, out);
        close_link(url, out);
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
    use super::{Block, blocks, code_lines};

    fn markdown(block: &Block) -> &str {
        match block {
            Block::Heading { markdown, .. }
            | Block::Paragraph { markdown }
            | Block::Item { markdown, .. } => markdown,
            Block::Code { text } => text,
            Block::Rule => "",
        }
    }

    /// What Slint makes of it: anything it cannot draw fails the whole string.
    fn styled(markdown: &str) -> slint::StyledText {
        slint::StyledText::from_markdown(markdown)
            .unwrap_or_else(|error| panic!("Slint refused {markdown:?}: {error}"))
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
                markdown: "**What\\'s Changed**".into(),
            }
        );
        assert_eq!(
            rendered[1],
            Block::Item {
                depth: 0,
                marker: "•".into(),
                markdown: concat!(
                    "fix\\(ui\\)\\: a fix by \\@zbndev in ",
                    "[https\\:\\/\\/github\\.com\\/zbndev\\/tidemark\\/pull\\/69]",
                    "(<https://github.com/zbndev/tidemark/pull/69>)",
                )
                .into(),
            },
            "a bare pull-request URL is a link, or the whole preview is dead text"
        );
        assert!(
            markdown(&rendered[2]).starts_with("**Full Changelog**\\: [https"),
            "got {:?}",
            rendered[2]
        );
        for block in &rendered {
            styled(markdown(block));
        }
    }

    #[test]
    fn emphasis_code_and_links_survive_as_styled_text() {
        let rendered = blocks("*Cards* keep `MIN_WIDTH`, see [the PR](https://example.com/a).");

        assert_eq!(
            markdown(&rendered[0]),
            "*Cards* keep ` MIN_WIDTH `\\, see [the PR](<https://example.com/a>)\\."
        );
        styled(markdown(&rendered[0]));
    }

    #[test]
    fn raw_html_is_dropped_and_everything_else_is_escaped() {
        let rendered = blocks("Use <b>&amp;</b> in \"quotes\" <!-- and a comment -->\n");

        assert_eq!(
            markdown(&rendered[0]),
            "Use \\& in \\\"quotes\\\"",
            "tags and comments go, their text stays, and nothing in it reaches Slint as \
             markup: one construct it cannot draw and it refuses the whole block"
        );
        styled(markdown(&rendered[0]));
    }

    #[test]
    fn text_that_looks_like_markdown_stays_text() {
        // Escaped, so not a list, a heading, a quote or a tag once it is a block's text.
        let rendered = blocks("Run \\*all\\* of `a`` b` then <u>x</u> 1\\. done # ok > yes\n");
        styled(markdown(&rendered[0]));
        assert!(
            markdown(&rendered[0]).contains("```` a`` b ````")
                || markdown(&rendered[0]).contains("``` a`` b ```"),
            "got {:?}",
            rendered[0]
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
                    markdown: "first".into(),
                },
                Block::Item {
                    depth: 0,
                    marker: "2.".into(),
                    markdown: "second".into(),
                },
                Block::Item {
                    depth: 1,
                    marker: "•".into(),
                    markdown: "nested".into(),
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
            markdown(&rendered[0]),
            concat!(
                "See [https\\:\\/\\/example\\.com\\/a\\_\\(b\\)](<https://example.com/a_(b)>)\\, ",
                "then [https\\:\\/\\/example\\.com\\/c](<https://example.com/c>)\\.",
            )
        );
        styled(markdown(&rendered[0]));
    }

    #[test]
    fn a_word_ending_in_a_scheme_is_not_a_link() {
        let rendered = blocks("nothttps://example.com is not a link\n");

        assert!(!markdown(&rendered[0]).contains("]("), "{:?}", rendered[0]);
    }

    #[test]
    fn hard_breaks_and_loose_items_become_lines_without_a_trailing_break() {
        let rendered = blocks("one  \ntwo\n\n- item\n\n  more\n");

        assert_eq!(markdown(&rendered[0]), "one\\\ntwo");
        assert_eq!(markdown(&rendered[1]), "item\\\nmore");
        styled(markdown(&rendered[0]));
        styled(markdown(&rendered[1]));
    }

    #[test]
    fn a_heading_is_bold_once_however_it_was_written() {
        let rendered = blocks("### **Full** `log`\n");

        assert_eq!(markdown(&rendered[0]), "**Full ` log `**");
        styled(markdown(&rendered[0]));
    }

    #[test]
    fn a_code_block_is_one_code_span_per_line() {
        let lines = code_lines("cargo test\n\n  `x`");

        assert_eq!(lines, "` cargo test `\\\n\u{a0}\\\n``   `x` ``");
        styled(&lines);
    }

    #[test]
    fn empty_notes_render_nothing_rather_than_a_blank_block() {
        assert!(blocks("").is_empty());
        assert!(blocks("\n\n   \n").is_empty());
    }
}
