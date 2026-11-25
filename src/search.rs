use crate::{
    CaseMode, Context, Coverage, Diagnostic, Error, Kind, Result, SearchHit, SearchOptions,
    SearchResult, Sort,
    api::{accepts, selected},
    index::{
        Document, IndexSession,
        coverage::{TOOL_RESULT_TAIL, mismatch},
    },
    read::python_whitespace,
};
use regex::{Regex, RegexBuilder};
use regex_syntax::ast::{self, Ast, ClassSetItem, parse::Parser};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
};

pub(crate) struct Pattern {
    pub(crate) case_sensitive: bool,
    matcher: Regex,
    prefilter: Option<Regex>,
}

impl Pattern {
    fn find_iter<'t>(&self, text: &'t str) -> impl Iterator<Item = regex::Match<'t>> {
        let possible = self
            .prefilter
            .as_ref()
            .is_none_or(|prefilter| prefilter.is_match(text));
        possible
            .then(|| self.matcher.find_iter(text))
            .into_iter()
            .flatten()
    }
}

pub(crate) fn pattern(options: &SearchOptions) -> Result<Pattern> {
    if !options.regex && options.query.is_empty() {
        return Err(Error::InvalidArgument(
            "literal query must not be empty".into(),
        ));
    }
    if options.max_results == 0 {
        return Err(Error::InvalidArgument(
            "max_results must be positive".into(),
        ));
    }
    let invalid = || Error::InvalidArgument("invalid or unsupported regular expression".into());
    let parsed = options
        .regex
        .then(|| {
            Parser::new()
                .parse_with_comments(&options.query)
                .map_err(|_| invalid())
        })
        .transpose()?;
    let case_sensitive = case_sensitive(
        options.case_mode,
        &options.query,
        parsed.as_ref().map(|parsed| &parsed.ast),
    );
    let build = |pattern: &str| {
        RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|_| invalid())
    };
    let body = if options.regex {
        options.query.clone()
    } else {
        regex::escape(&options.query)
    };
    let plain = build(&body)?;
    if !options.whole_word {
        return Ok(Pattern {
            case_sensitive,
            matcher: plain,
            prefilter: None,
        });
    }
    // A trailing (?x) comment would otherwise swallow the wrapper's closing parenthesis.
    let tail = if parsed.is_some_and(|parsed| {
        parsed
            .comments
            .iter()
            .any(|comment| comment.span.end.offset == body.len())
    }) {
        "\n"
    } else {
        ""
    };
    Ok(Pattern {
        case_sensitive,
        matcher: build(&format!(r"\b{{start-half}}(?:{body}{tail})\b{{end-half}}"))?,
        prefilter: Some(plain),
    })
}

struct UppercaseLiteral(bool);

impl UppercaseLiteral {
    const fn literal(&mut self, literal: &ast::Literal) {
        self.0 |= literal.c.is_uppercase();
    }
}

impl ast::Visitor for UppercaseLiteral {
    type Output = bool;
    type Err = Infallible;
    fn finish(self) -> std::result::Result<bool, Infallible> {
        Ok(self.0)
    }
    fn visit_pre(&mut self, ast: &Ast) -> std::result::Result<(), Infallible> {
        if let Ast::Literal(literal) = ast {
            self.literal(literal);
        }
        Ok(())
    }
    fn visit_class_set_item_pre(
        &mut self,
        item: &ClassSetItem,
    ) -> std::result::Result<(), Infallible> {
        match item {
            ClassSetItem::Literal(literal) => self.literal(literal),
            ClassSetItem::Range(range) => {
                self.literal(&range.start);
                self.literal(&range.end);
            }
            _ => {}
        }
        Ok(())
    }
}

fn case_sensitive(mode: CaseMode, query: &str, regex: Option<&Ast>) -> bool {
    match (mode, regex) {
        (CaseMode::Sensitive, _) => true,
        (CaseMode::Insensitive, _) => false,
        (CaseMode::Smart, None) => query.chars().any(char::is_uppercase),
        (CaseMode::Smart, Some(ast)) => {
            let Ok(found) = ast::visit(ast, UppercaseLiteral(false));
            found
        }
    }
}

pub(crate) fn run(
    ctx: &Context,
    index: &IndexSession,
    options: &SearchOptions,
    pattern: &Pattern,
) -> Result<SearchResult> {
    let mut metadata = BTreeMap::new();
    index.visit_sessions(&mut |session| {
        if accepts(ctx, &options.filter, &session) {
            metadata.insert(session.key.reference(), session);
        }
        Ok(())
    })?;
    let mut results = Vec::new();
    let mut current: Option<SearchHit> = None;
    let keys: Vec<_> = metadata
        .values()
        .map(|session| session.key.clone())
        .collect();
    index.documents(
        &keys,
        &options.filter.kinds,
        (!options.regex).then_some((options.query.as_str(), pattern.case_sensitive)),
        &mut |document| {
            if current
                .as_ref()
                .is_some_and(|hit| hit.session.key != document.session)
            {
                retain(&mut results, current.take().unwrap(), options);
            }
            if current.is_none() {
                let Some(session) = metadata.remove(&document.session.reference()) else {
                    return Ok(());
                };
                current = Some(SearchHit {
                    session,
                    score: 0,
                    matches: 0,
                    by_kind: BTreeMap::new(),
                    snippets: Vec::new(),
                });
            }
            accumulate(current.as_mut().unwrap(), &document, options, pattern);
            Ok(())
        },
    )?;
    if let Some(hit) = current {
        retain(&mut results, hit, options);
    }
    Ok(SearchResult {
        query: options.query.clone(),
        agents: selected(&options.filter.agents).into_iter().collect(),
        sort: options.sort,
        count: results.len(),
        results,
    })
}

pub(crate) fn coverage_notes(
    index: Coverage,
    config: Coverage,
    explicit: bool,
    kinds: &BTreeSet<Kind>,
) -> Vec<Diagnostic> {
    let note = |code: &str, message: String| Diagnostic {
        agent: None,
        source: None,
        code: code.into(),
        message,
    };
    let tail = format!("{} KB", TOOL_RESULT_TAIL / 1024);
    let limited: Vec<String> = [
        (
            Kind::Meta,
            "--kind meta does not search reasoning in compact coverage".to_string(),
        ),
        (
            Kind::Tool,
            format!(
                "--kind tool searches only the last {tail} of each tool result in compact coverage"
            ),
        ),
    ]
    .into_iter()
    .filter(|(kind, _)| kinds.contains(kind))
    .map(|(_, text)| text)
    .collect();
    let coverage = match (index, limited.is_empty()) {
        (Coverage::Full, _) => None,
        (Coverage::Compact, false) => Some(note(
            "coverage_excluded",
            format!(
                "{}; run `slimetrail index --coverage full` to search everything",
                limited.join("; ")
            ),
        )),
        (Coverage::Compact, true) => (!explicit).then(|| {
            note(
                "search_coverage",
                format!(
                    "Compact coverage: reasoning is not searched, and only the last {tail} of each tool result is searched; run `slimetrail index --coverage full` to search everything"
                ),
            )
        }),
    };
    coverage
        .into_iter()
        .chain(mismatch(index, config).map(|text| note("coverage_mismatch", text.into())))
        .collect()
}

fn retain(results: &mut Vec<SearchHit>, hit: SearchHit, options: &SearchOptions) {
    if hit.matches == 0 {
        return;
    }
    let position = results
        .binary_search_by(|other| compare(other, &hit, options.sort))
        .unwrap_or_else(|i| i);
    if position < options.max_results {
        results.insert(position, hit);
        results.truncate(options.max_results);
    }
}

fn compare(left: &SearchHit, right: &SearchHit, sort: Sort) -> Ordering {
    let score = right.score.cmp(&left.score);
    let recent = right
        .session
        .updated_at
        .unwrap_or(0)
        .cmp(&left.session.updated_at.unwrap_or(0));
    match sort {
        Sort::Score => score.then(recent),
        Sort::Recent => recent.then(score),
    }
    .then_with(|| left.session.key.agent.cmp(&right.session.key.agent))
    .then_with(|| left.session.key.id.cmp(&right.session.key.id))
}

fn accumulate(
    hit: &mut SearchHit,
    document: &Document,
    options: &SearchOptions,
    pattern: &Pattern,
) {
    if document.kind != "title"
        && !options.filter.kinds.is_empty()
        && !options
            .filter
            .kinds
            .iter()
            .any(|kind| kind.as_str() == document.kind)
    {
        return;
    }
    let weight = match document.kind.as_str() {
        "title" => 5,
        "message" => 3,
        "meta" => 2,
        _ => 1,
    };
    for found in pattern.find_iter(&document.text).filter(|m| !m.is_empty()) {
        hit.matches += 1;
        hit.score += weight;
        *hit.by_kind.entry(document.kind.clone()).or_default() += 1;
        if hit.snippets.len() < options.max_snippets {
            let label = if document.kind == "title" {
                "title".into()
            } else {
                format!(
                    "{}/{}",
                    document.role.as_deref().unwrap_or(""),
                    document.kind
                )
            };
            hit.snippets.push(format!(
                "[{label}] {}",
                snippet(
                    &document.text,
                    found.start(),
                    found.end(),
                    options.context_chars
                )
            ));
        }
    }
}

fn snippet(text: &str, start: usize, end: usize, context: usize) -> String {
    let left = text[..start]
        .char_indices()
        .rev()
        .nth(context.saturating_sub(1))
        .map_or(0, |(i, _)| i);
    let left = if context == 0 { start } else { left };
    let right = text[end..]
        .char_indices()
        .nth(context)
        .map_or(text.len(), |(i, _)| end + i);
    let body = text[left..right]
        .split(python_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}{body}{}",
        if left > 0 { "…" } else { "" },
        if right < text.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(index: Coverage, config: Coverage, explicit: bool, kinds: &[Kind]) -> Vec<String> {
        coverage_notes(index, config, explicit, &kinds.iter().copied().collect())
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn coverage_notes_follow_explicit_config_kinds_and_mismatch() {
        let compact = Coverage::Compact;
        let full = Coverage::Full;
        assert_eq!(codes(compact, compact, false, &[]), ["search_coverage"]);
        assert_eq!(codes(compact, compact, true, &[]), Vec::<String>::new());
        assert_eq!(
            codes(compact, compact, true, &[Kind::Tool]),
            ["coverage_excluded"]
        );
        assert_eq!(
            codes(compact, full, true, &[Kind::Meta]),
            ["coverage_excluded", "coverage_mismatch"]
        );
        assert_eq!(codes(full, compact, false, &[]), ["coverage_mismatch"]);
        assert_eq!(codes(full, full, true, &[Kind::Meta]), Vec::<String>::new());
    }

    #[test]
    fn smart_case_counts_only_literal_uppercase() {
        for (query, regex, expected) in [
            ("foo", false, false),
            ("Foo", false, true),
            ("ÉCOLE", false, true),
            ("école", false, false),
            (r"\w+", true, false),
            (r"\W", true, false),
            (r"\p{Lu}", true, false),
            ("[[:upper:]]", true, false),
            ("[A-Z]", true, true),
            ("[a[B]]", true, true),
            (r"\x41", true, true),
            (r"\x6A", true, false),
            ("(?i)foo", true, false),
            ("(?x)foo # Comment", true, false),
            ("foo|Bar", true, true),
        ] {
            let ast = regex.then(|| Parser::new().parse(query).unwrap());
            assert_eq!(
                case_sensitive(CaseMode::Smart, query, ast.as_ref()),
                expected,
                "{query} regex={regex}"
            );
        }
    }

    #[test]
    fn explicit_case_modes_ignore_the_query() {
        assert!(case_sensitive(CaseMode::Sensitive, "foo", None));
        assert!(!case_sensitive(CaseMode::Insensitive, "Foo", None));
    }
}
