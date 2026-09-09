//! The plugin's provider mark, reduced to a static picture.
//!
//! An SVG is a document format with scripting, external references and stylesheets in it,
//! and this one arrives from a file a stranger wrote. So it is not "checked": it is parsed,
//! matched against an allowlist of elements and attributes sufficient for a provider mark,
//! and *rewritten*. Anything the allowlist does not name is either refused — where keeping
//! it would change what the picture does — or dropped, where it is merely decoration.
//!
//! Refused rather than dropped, deliberately: a mark that carried a script is a file whose
//! author's intent we do not want to guess at, and telling them so is more useful than
//! silently showing them a different picture.

use super::{PluginError, limits};
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesEnd, BytesStart, Event};
use quick_xml::{Reader, Writer, XmlVersion};

/// The largest mark accepted, well under the file bound so a mark alone cannot fill a file.
const MAX_SVG_BYTES: usize = 128 * 1024;
/// How deep grouping may nest. A provider mark needs a handful; a thousand is an attack.
const MAX_DEPTH: usize = 32;

/// Elements a provider mark may use.
const ELEMENTS: &[&str] = &[
    "svg", "g", "path", "rect", "circle", "ellipse", "line", "polyline", "polygon", "title",
];

/// Attributes accepted on any accepted element.
const ATTRIBUTES: &[&str] = &[
    "viewBox",
    "width",
    "height",
    "xmlns",
    "transform",
    "d",
    "x",
    "y",
    "x1",
    "y1",
    "x2",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "points",
    "fill",
    "fill-rule",
    "fill-opacity",
    "opacity",
];

/// Elements whose presence refuses the whole document.
const HOSTILE_ELEMENTS: &[&str] = &[
    "script",
    "style",
    "foreignobject",
    "image",
    "use",
    "a",
    "animate",
    "animatemotion",
    "animatetransform",
    "set",
    "filter",
    "clippath",
    "mask",
    "pattern",
    "lineargradient",
    "radialgradient",
    "symbol",
    "marker",
    "switch",
    "text",
    "textpath",
    "tspan",
    "iframe",
    "audio",
    "video",
    "handler",
    "listener",
];

/// Accepts a mark and returns canonical bytes, or says which rule it broke.
pub fn sanitize(source: &str) -> Result<String, PluginError> {
    if source.len() > MAX_SVG_BYTES {
        return Err(PluginError::TooLarge {
            what: "the provider mark",
            found: source.len(),
            limit: MAX_SVG_BYTES,
        });
    }
    let lowered = source.to_ascii_lowercase();
    for forbidden in ["<!doctype", "<!entity", "<?xml-stylesheet"] {
        if lowered.contains(forbidden) {
            return Err(svg(format!(
                "{forbidden} is not allowed in a provider mark"
            )));
        }
    }

    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    let mut writer = Writer::new(Vec::new());
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut in_title = false;

    loop {
        match reader
            .read_event()
            .map_err(|error| svg(error.to_string()))?
        {
            Event::Eof => break,
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if !root_seen && name != "svg" {
                    return Err(svg("the document root must be <svg>".to_owned()));
                }
                root_seen = true;
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(svg(format!("nesting deeper than {MAX_DEPTH} elements")));
                }
                in_title = name == "title";
                writer
                    .write_event(Event::Start(accept(&name, &start)?))
                    .map_err(|error| svg(error.to_string()))?;
            }
            Event::Empty(start) => {
                let name = local_name(start.name().as_ref());
                if !root_seen && name != "svg" {
                    return Err(svg("the document root must be <svg>".to_owned()));
                }
                root_seen = true;
                writer
                    .write_event(Event::Empty(accept(&name, &start)?))
                    .map_err(|error| svg(error.to_string()))?;
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                depth = depth.saturating_sub(1);
                in_title = false;
                writer
                    .write_event(Event::End(BytesEnd::new(name)))
                    .map_err(|error| svg(error.to_string()))?;
            }
            // Text survives only inside <title>, which is the one place a mark has words.
            // Anywhere else it draws nothing, so keeping it would only carry a stranger's
            // bytes forward for no picture.
            Event::Text(text) if in_title => {
                writer
                    .write_event(Event::Text(text))
                    .map_err(|error| svg(error.to_string()))?;
            }
            // Comments, processing instructions, CDATA and declarations are dropped: none of
            // them draws anything, and each is a place something executable has hidden.
            _ => {}
        }
    }

    if !root_seen {
        return Err(svg("no <svg> element was found".to_owned()));
    }
    String::from_utf8(writer.into_inner()).map_err(|error| svg(error.to_string()))
}

/// One element, with only the attributes the allowlist names and only local references.
fn accept(name: &str, start: &BytesStart<'_>) -> Result<BytesStart<'static>, PluginError> {
    if HOSTILE_ELEMENTS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(svg(format!("<{name}> is not allowed in a provider mark")));
    }
    if !ELEMENTS.contains(&name) {
        return Err(svg(format!("<{name}> is not part of the accepted subset")));
    }
    let mut element = BytesStart::new(name.to_owned());
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|error| svg(error.to_string()))?;
        let key = attribute.key.as_ref().to_owned();
        let lowered = key.to_ascii_lowercase();
        if lowered.starts_with("on") || lowered.contains("href") || lowered.starts_with("xlink:") {
            return Err(svg(format!("{key} is not allowed in a provider mark")));
        }
        if !ATTRIBUTES.contains(&key.as_str()) {
            continue;
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| svg(error.to_string()))?;
        if value.to_ascii_lowercase().contains("url(")
            || value.to_ascii_lowercase().contains("data:")
        {
            return Err(svg(format!(
                "{key} references something outside the document"
            )));
        }
        // Written through `Attribute::from`, which escapes: the value was unescaped to be
        // inspected, and putting those bytes back raw would let a `&quot;` in the source
        // close the attribute and open another one the allowlist never saw.
        element.push_attribute(Attribute::from((key.as_str(), value.as_ref())));
    }
    Ok(element)
}

/// The element name without its namespace prefix.
fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

/// One refusal, bounded so a hostile document cannot write the diagnostic.
fn svg(reason: String) -> PluginError {
    let mut reason = reason;
    reason.truncate(limits::MAX_ERROR_BYTES);
    PluginError::Svg { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <g transform="translate(2 2)"><path fill="currentColor" d="M0 0h10v10H0z"/></g>
  <circle cx="8" cy="8" r="4" fill="#336699"/>
</svg>"##;

    #[test]
    fn a_conservative_static_mark_is_accepted_and_serialized_canonically() {
        let clean = sanitize(MARK).expect("the documented subset is accepted");
        assert!(clean.starts_with("<svg"), "{clean}");
        assert!(clean.contains("viewBox=\"0 0 64 64\""));
        assert!(clean.contains("fill=\"currentColor\""));
        assert!(clean.contains("<circle"));
        assert_eq!(
            sanitize(&clean).expect("idempotent"),
            clean,
            "sanitizing twice changes nothing"
        );
    }

    #[test]
    fn executable_and_interactive_content_is_refused() {
        for hostile in [
            r#"<svg><script>fetch("http://x")</script></svg>"#,
            r#"<svg><path onload="x()" d="M0 0"/></svg>"#,
            r#"<svg><animate attributeName="x" to="9"/></svg>"#,
            r#"<svg><foreignObject><div/></foreignObject></svg>"#,
            r#"<svg><filter id="f"/><path filter="url(#f)" d="M0 0"/></svg>"#,
            r#"<svg><style>path{fill:red}</style></svg>"#,
            r#"<svg><a href="http://x"><path d="M0 0"/></a></svg>"#,
        ] {
            assert!(
                matches!(sanitize(hostile), Err(PluginError::Svg { .. })),
                "{hostile}"
            );
        }
    }

    #[test]
    fn every_reference_out_of_the_document_is_refused() {
        for hostile in [
            r#"<svg><image href="http://example.test/a.png"/></svg>"#,
            r#"<svg><image xlink:href="data:image/png;base64,AAA"/></svg>"#,
            r#"<svg><path fill="url(http://example.test/g)" d="M0 0"/></svg>"#,
            r##"<svg><use href="#other"/></svg>"##,
            r#"<?xml version="1.0"?><!DOCTYPE svg SYSTEM "http://example.test/svg.dtd"><svg/>"#,
            r#"<svg><!ENTITY x SYSTEM "file:///etc/passwd"></svg>"#,
        ] {
            assert!(
                matches!(sanitize(hostile), Err(PluginError::Svg { .. })),
                "{hostile}"
            );
        }
    }

    #[test]
    fn a_document_that_is_not_an_svg_root_is_refused() {
        for hostile in [r#"<html><svg/></html>"#, "not xml at all", "", r#"<svg"#] {
            assert!(sanitize(hostile).is_err(), "{hostile:?}");
        }
    }

    #[test]
    fn an_oversized_mark_is_refused_before_it_is_parsed() {
        let huge = format!("<svg>{}</svg>", "<path d=\"M0 0\"/>".repeat(60_000));
        assert!(matches!(sanitize(&huge), Err(PluginError::TooLarge { .. })));
    }

    #[test]
    fn nesting_deep_enough_to_recurse_is_refused() {
        let deep = format!("<svg>{}{}</svg>", "<g>".repeat(200), "</g>".repeat(200));
        assert!(matches!(sanitize(&deep), Err(PluginError::Svg { .. })));
    }

    #[test]
    fn an_attribute_outside_the_accepted_set_is_dropped_rather_than_kept() {
        let clean = sanitize(r#"<svg><path d="M0 0" data-note="hi" tabindex="1"/></svg>"#)
            .expect("the geometry is fine");
        assert!(!clean.contains("data-note"));
        assert!(!clean.contains("tabindex"));
        assert!(clean.contains("d=\"M0 0\""));
    }

    #[test]
    fn an_escaped_quote_cannot_smuggle_a_second_attribute_out_of_a_value() {
        let clean = sanitize(r#"<svg><path d="M0 0&quot; onload=&quot;x()"/></svg>"#)
            .expect("the value is a value, however it is spelled");
        assert!(
            !clean.contains("onload=\"x()\""),
            "an unescaped value would reopen the attribute list: {clean}"
        );
        assert_eq!(
            sanitize(&clean).expect("idempotent"),
            clean,
            "escaping is stable across passes"
        );
    }
}
