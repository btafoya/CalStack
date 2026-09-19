//! Small DAV request bodies parsed by local name, so `<D:sync-token>`,
//! `<sync-token xmlns="DAV:">` and any other prefix all match. No external
//! entities are resolved (xml-rs).

use xmltree::Element;

/// Bodies larger than this are not DAV property/report requests worth parsing.
const MAX_BODY: usize = 1 << 20;

pub(crate) fn parse(body: &str) -> Option<Element> {
    if body.len() > MAX_BODY {
        return None;
    }
    Element::parse(body.as_bytes()).ok()
}

/// First element (depth-first, `el` included) with this local name.
pub(crate) fn find<'a>(el: &'a Element, name: &str) -> Option<&'a Element> {
    if el.name == name {
        return Some(el);
    }
    el.children
        .iter()
        .filter_map(|n| n.as_element())
        .find_map(|c| find(c, name))
}

pub(crate) fn text(el: &Element) -> String {
    el.get_text()
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_any_prefix() {
        for body in [
            r#"<D:sync-collection xmlns:D="DAV:"><D:sync-token>42</D:sync-token></D:sync-collection>"#,
            r#"<sync-collection xmlns="DAV:"><sync-token>42</sync-token></sync-collection>"#,
            r#"<A:sync-collection xmlns:A="DAV:"><A:sync-token> 42 </A:sync-token></A:sync-collection>"#,
        ] {
            let root = parse(body).unwrap();
            assert_eq!(text(find(&root, "sync-token").unwrap()), "42");
        }
    }

    #[test]
    fn rejects_oversized_and_garbage() {
        assert!(parse("<a><b></a>").is_none());
        assert!(parse(&format!("<a>{}</a>", "x".repeat(MAX_BODY))).is_none());
    }
}
