use crate::{Coverage, Error, Kind, Result, Searchable};

pub const TOOL_RESULT_TAIL: usize = 2048;

pub fn searchable_start(
    coverage: Coverage,
    kind: Kind,
    subtype: Option<&str>,
    searchable: Searchable,
    text: &str,
) -> Option<usize> {
    match (coverage, searchable) {
        (_, Searchable::Duplicate) => None,
        (Coverage::Compact, _) if kind == Kind::Meta && subtype == Some("reasoning") => None,
        (Coverage::Compact, Searchable::ToolResult) => Some(tail_start(text)),
        (Coverage::Full, _) | (Coverage::Compact, Searchable::Text) => Some(0),
    }
}

pub const fn tail_start(text: &str) -> usize {
    let mut start = text.len().saturating_sub(TOOL_RESULT_TAIL);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

pub(crate) const fn stored_split(searchable: Searchable, text: &str) -> usize {
    match searchable {
        Searchable::ToolResult => tail_start(text),
        Searchable::Text | Searchable::Duplicate => text.len(),
    }
}

pub const fn mismatch(index: Coverage, config: Coverage) -> Option<&'static str> {
    match (index, config) {
        (Coverage::Compact, Coverage::Full) => Some(
            "The config asks for full coverage but the index is compact; run `slimetrail index` to rebuild it",
        ),
        (Coverage::Full, Coverage::Compact) => Some(
            "The config asks for compact coverage but the index is full; run `slimetrail index` to rebuild it",
        ),
        _ => None,
    }
}

pub(crate) const fn searchable_name(searchable: Searchable) -> &'static str {
    match searchable {
        Searchable::Text => "text",
        Searchable::ToolResult => "tool_result",
        Searchable::Duplicate => "duplicate",
    }
}

pub(crate) fn parse_searchable(name: &str) -> Result<Searchable> {
    match name {
        "text" => Ok(Searchable::Text),
        "tool_result" => Ok(Searchable::ToolResult),
        "duplicate" => Ok(Searchable::Duplicate),
        _ => Err(Error::InvalidArgument(
            "entry searchability is invalid".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_keeps_tool_result_tails_and_drops_reasoning() {
        let long = format!("{}{}", "é".repeat(1500), "x".repeat(10));
        let start = searchable_start(
            Coverage::Compact,
            Kind::Tool,
            None,
            Searchable::ToolResult,
            &long,
        )
        .unwrap();
        assert!(long.is_char_boundary(start));
        assert_eq!(long.len() - start, TOOL_RESULT_TAIL);
        let shifted = format!("{long}y");
        let start = searchable_start(
            Coverage::Compact,
            Kind::Tool,
            None,
            Searchable::ToolResult,
            &shifted,
        )
        .unwrap();
        assert_eq!(shifted.len() - start, TOOL_RESULT_TAIL - 1);
        assert_eq!(
            searchable_start(
                Coverage::Compact,
                Kind::Tool,
                None,
                Searchable::ToolResult,
                "short"
            ),
            Some(0)
        );
        assert_eq!(
            searchable_start(
                Coverage::Compact,
                Kind::Meta,
                Some("reasoning"),
                Searchable::Text,
                "x"
            ),
            None
        );
        assert_eq!(
            searchable_start(
                Coverage::Compact,
                Kind::Meta,
                Some("summary"),
                Searchable::Text,
                "x"
            ),
            Some(0)
        );
        assert_eq!(
            searchable_start(
                Coverage::Full,
                Kind::Meta,
                Some("reasoning"),
                Searchable::Text,
                "x"
            ),
            Some(0)
        );
        assert_eq!(
            searchable_start(
                Coverage::Full,
                Kind::Tool,
                None,
                Searchable::ToolResult,
                &long
            ),
            Some(0)
        );
        for coverage in [Coverage::Compact, Coverage::Full] {
            assert_eq!(
                searchable_start(coverage, Kind::Tool, None, Searchable::Duplicate, "x"),
                None
            );
        }
    }
}
