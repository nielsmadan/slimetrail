#[allow(dead_code)]
mod query_support;
#[allow(dead_code)]
mod support;
use query_support::*;
use serde_json::json;
use slimetrail::*;
use std::{
    collections::BTreeSet,
    fs,
    time::{Duration, SystemTime},
};

#[test]
fn indexed_literals_equal_independent_scalar_matcher() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let texts = [
        "ABC abc abab a.b a*b a?b \"quoted\" 100% under_score /a/b c:\\path",
        "ſearch \u{212a}elvin Σ σ ς café CAFÉ",
        "prefix\0needle suffix",
        "no matches",
        "aaa aaaa",
        "naïve 🐌 🐌🐌",
    ];
    for (i, text) in texts.iter().enumerate() {
        claude(
            &ctx,
            &format!("s{i}"),
            None,
            None,
            &[event("user", text, None)],
        );
    }
    for query in [
        "a",
        "ab",
        "abc",
        "a.b",
        "a*b",
        "a?b",
        "\"quoted\"",
        "100%",
        "under_score",
        "/a/b",
        "c:\\path",
        "search",
        "kelvin",
        "σ",
        "café",
        "needle",
        "\0needle",
        "aaa",
        "🐌",
        "İ",
        "missing",
    ] {
        for sensitive in [false, true] {
            let mut opt = options(query);
            opt.case_mode = case_mode(sensitive);
            let expected: std::collections::BTreeMap<_, _> = texts
                .iter()
                .enumerate()
                .filter_map(|(i, text)| {
                    let count = brute(text, query, sensitive);
                    (count > 0).then_some((format!("s{i}"), count))
                })
                .collect();
            assert_eq!(
                session_matches(&ctx, &opt),
                expected,
                "{query:?}, case_sensitive={sensitive}"
            );
        }
    }
}

fn brute(text: &str, query: &str, sensitive: bool) -> u64 {
    count_matches(text, query, sensitive, false)
}

fn brute_word(text: &str, query: &str, sensitive: bool) -> u64 {
    count_matches(text, query, sensitive, true)
}

fn count_matches(text: &str, query: &str, sensitive: bool, word: bool) -> u64 {
    fn fold(c: char) -> char {
        match c {
            'İ' => 'İ',
            'ſ' => 's',
            '\u{212a}' => 'k',
            'Σ' | 'ς' => 'σ',
            _ => c.to_lowercase().next().unwrap(),
        }
    }
    let convert = |s: &str| {
        s.chars()
            .map(|c| if sensitive { c } else { fold(c) })
            .collect::<Vec<_>>()
    };
    let word_char = regex::Regex::new(r"\w").unwrap();
    let is_word = |c: Option<&char>| c.is_some_and(|c| word_char.is_match(&c.to_string()));
    let original = text.chars().collect::<Vec<_>>();
    let text = convert(text);
    let query = convert(query);
    let mut position = 0;
    let mut count = 0;
    while position + query.len() <= text.len() {
        let end = position + query.len();
        if text[position..end] == query
            && (!word
                || (!is_word(position.checked_sub(1).and_then(|i| original.get(i)))
                    && !is_word(original.get(end))))
        {
            count += 1;
            position = end;
        } else {
            position += 1;
        }
    }
    count
}

fn session_matches(ctx: &Context, opt: &SearchOptions) -> std::collections::BTreeMap<String, u64> {
    search(ctx, opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap()
        .results
        .into_iter()
        .map(|h| (h.session.key.id, h.matches))
        .collect()
}

fn word_texts(ctx: &Context) -> [&'static str; 7] {
    let texts = [
        "ABC abc abab a.b a*b \"quoted\" 100% under_score /a/b c:\\path",
        "ſearch \u{212a}elvin Σ σ ς café CAFÉ cafés",
        "prefix\0needle suffix needle2 needle_x 2needle NEEDLE",
        "x---- a.ba.b aaa aaaa aa",
        "naïve 🐌needle🐌 🐌 use Result<T> here Result<T>x",
        "no matches",
        "\"quoted\"x x\"quoted\" 100%5 a/a/b /a/b/ --- ---",
    ];
    for (i, text) in texts.iter().enumerate() {
        claude(
            ctx,
            &format!("s{i}"),
            None,
            None,
            &[event("user", text, None)],
        );
    }
    texts
}

#[test]
fn word_literals_equal_independent_matcher() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let texts = word_texts(&ctx);
    for query in [
        "\"quoted\"",
        "/a/b",
        "100%",
        "a.b",
        "Result<T>",
        "---",
        "needle",
        "kelvin",
        "aa",
        "abc",
        "search",
        "café",
        "naïve",
        "σ",
        "🐌",
        "under_score",
    ] {
        for sensitive in [false, true] {
            let mut opt = options(query);
            opt.case_mode = case_mode(sensitive);
            opt.whole_word = true;
            let expected: std::collections::BTreeMap<_, _> = texts
                .iter()
                .enumerate()
                .filter_map(|(i, text)| {
                    let count = brute_word(text, query, sensitive);
                    (count > 0).then_some((format!("s{i}"), count))
                })
                .collect();
            assert_eq!(
                session_matches(&ctx, &opt),
                expected,
                "{query:?}, case_sensitive={sensitive}"
            );
        }
    }
}

#[test]
fn word_oracle_retries_after_a_rejected_candidate() {
    assert_eq!(brute_word("x----", "---", true), 1);
}

#[test]
fn word_regex_equals_word_literal() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    word_texts(&ctx);
    for query in [
        "needle",
        "abc",
        "a.b",
        "Result<T>",
        "---",
        "kelvin",
        "café",
        "aa",
        "σ",
        "/a/b",
    ] {
        for sensitive in [false, true] {
            let mut literal = options(query);
            literal.case_mode = case_mode(sensitive);
            literal.whole_word = true;
            let mut pattern = literal.clone();
            pattern.regex = true;
            pattern.query = regex::escape(query);
            let literal = search(&ctx, &literal, &RefreshOptions::QUERY)
                .unwrap()
                .data
                .unwrap();
            let pattern = search(&ctx, &pattern, &RefreshOptions::QUERY)
                .unwrap()
                .data
                .unwrap();
            assert_eq!(literal.count, pattern.count, "{query:?}, {sensitive}");
            assert_eq!(literal.results, pattern.results, "{query:?}, {sensitive}");
        }
    }
}

#[test]
fn word_with_invalid_regex_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let mut opt = options("(");
    opt.regex = true;
    opt.whole_word = true;
    assert!(matches!(
        search(&ctx, &opt, &RefreshOptions::QUERY),
        Err(Error::InvalidArgument(message)) if message == "invalid or unsupported regular expression"
    ));
}

#[test]
fn word_regex_wrapping_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[event("user", "foo food xfoo foo bar", None)],
    );
    let mut opt = options("(?x)foo # tail");
    opt.regex = true;
    opt.whole_word = true;
    assert_eq!(session_matches(&ctx, &opt), [("one".into(), 2)].into());
    opt.query = "foo|bar".into();
    assert_eq!(session_matches(&ctx, &opt), [("one".into(), 3)].into());
    opt.query = "foo)|(?:bar".into();
    assert!(matches!(
        search(&ctx, &opt, &RefreshOptions::QUERY),
        Err(Error::InvalidArgument(message)) if message == "invalid or unsupported regular expression"
    ));
}

#[test]
fn word_spot_checks() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for (id, text) in [
        ("code", "rust code"),
        ("trust", "trust"),
        ("rusty", "rusty"),
        ("underscore", "rust_"),
        ("generic", "use Result<T> here"),
        ("accent", "naïve na"),
    ] {
        claude(&ctx, id, None, None, &[event("user", text, None)]);
    }
    let mut opt = options("rust");
    opt.whole_word = true;
    assert_eq!(session_matches(&ctx, &opt), [("code".into(), 1)].into());
    opt.query = "Result<T>".into();
    assert_eq!(session_matches(&ctx, &opt), [("generic".into(), 1)].into());
    opt.query = "na".into();
    assert_eq!(session_matches(&ctx, &opt), [("accent".into(), 1)].into());
}

#[test]
fn word_regex_counts_only_nonempty_matches() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", None, None, &[event("user", " aa xaa a", None)]);
    let mut opt = options("a*");
    opt.regex = true;
    opt.whole_word = true;
    assert_eq!(session_matches(&ctx, &opt), [("one".into(), 2)].into());
}

#[test]
fn inline_case_flags_override_the_case_mode() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[event("user", "NEEDLE Needle needle", None)],
    );
    let mut opt = options("(?-i)Needle");
    opt.regex = true;
    assert_eq!(session_matches(&ctx, &opt), [("one".into(), 1)].into());
    opt.query = "(?i)needle".into();
    opt.case_mode = CaseMode::Sensitive;
    assert_eq!(session_matches(&ctx, &opt), [("one".into(), 3)].into());
}

#[test]
fn native_title_once_kind_independent_and_derived_only_displayed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "native",
        Some("needle needle"),
        None,
        &[
            event("user", "needle", None),
            json!({"type":"future","text":"needle"}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_result","content":"needle"}]}}),
        ],
    );
    claude(
        &ctx,
        "derived",
        None,
        None,
        &[event("user", "needle", None)],
    );
    let opt = options("needle");
    let result = search(&ctx, &opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.results[0].session.key.id, "native");
    assert_eq!(result.results[0].score, 16);
    assert_eq!(result.results[0].matches, 5);
    assert_eq!(
        result.results[0].snippets,
        [
            "[title] needle needle",
            "[title] needle needle",
            "[user/message] needle"
        ]
    );
    assert_eq!(result.results[1].score, 3);
    assert_eq!(result.results[1].by_kind.len(), 1);
    let mut opt = opt;
    opt.filter.kinds = BTreeSet::from([Kind::Tool]);
    let result = search(&ctx, &opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.count, 1);
    assert_eq!(result.results[0].score, 11);
    assert_eq!(result.results[0].by_kind["title"], 2);
}

#[test]
fn regex_is_explicit_skips_zero_width_and_rejects_unsupported_syntax() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[event("user", "aaa a.b axb", None)],
    );
    assert_eq!(
        search(&ctx, &options("a.b"), &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .results[0]
            .matches,
        1
    );
    let mut opt = options("a.b");
    opt.regex = true;
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .results[0]
            .matches,
        2
    );
    opt.query = "a*".into();
    let result = search(&ctx, &opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.results[0].matches, 3);
    opt.query = "^|$".into();
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .count,
        0
    );
    for query in ["(?=a)", "(a)\\1", "["] {
        opt.query = query.into();
        assert!(search(&ctx, &opt, &RefreshOptions::QUERY).is_err());
    }
    assert!(search(&ctx, &options(""), &RefreshOptions::QUERY).is_err());
}

#[test]
fn snippets_use_scalar_context_whitespace_and_bounded_limits() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[event("user", "🐌αβ\n\tNEEDLE\u{1c}γδε🐌", None)],
    );
    let mut opt = options("needle");
    opt.context_chars = 2;
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .results[0]
            .snippets,
        ["[user/message] …NEEDLE γ…"]
    );
    opt.context_chars = 0;
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .results[0]
            .snippets,
        ["[user/message] …NEEDLE…"]
    );
    opt.max_snippets = 0;
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .results[0]
            .snippets
            .len(),
        0
    );
}

#[test]
fn ordering_and_top_limits_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for (id, text, time) in [
        ("z", "needle needle", "2026-01-01T00:00:00Z"),
        ("b", "needle", "2026-02-01T00:00:00Z"),
        ("a", "needle", "2026-02-01T00:00:00Z"),
        ("c", "needle needle", "2026-02-01T00:00:00Z"),
    ] {
        claude(&ctx, id, None, None, &[event("user", text, Some(time))]);
    }
    let mut opt = options("needle");
    opt.max_results = 3;
    let ids = |r: SearchResult| {
        r.results
            .into_iter()
            .map(|h| h.session.key.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()),
        ["c", "z", "a"]
    );
    opt.sort = Sort::Recent;
    assert_eq!(
        ids(search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()),
        ["c", "a", "b"]
    );
}

#[test]
fn scope_app_and_source_mtime_follow_reference_rules() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = context(dir.path());
    ctx.now_ms = 200 * 86_400_000;
    let paths = [
        ("current", Some("/fixture/project")),
        ("sibling", Some("/fixture/other")),
        ("descendant", Some("/fixture/project/child")),
        ("outside", Some("/outside/project")),
        ("unknown", None),
    ];
    for (id, cwd) in paths {
        let path = claude(
            &ctx,
            id,
            None,
            cwd,
            &[event("user", "needle", Some("1970-01-01T00:00:00Z"))],
        );
        fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(17_193_600))
            .unwrap();
    }
    let mut opt = options("needle");
    opt.filter.days = Some(1);
    opt.filter.scope = Scope::Siblings;
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .count,
        2
    );
    opt.filter.scope = Scope::Current;
    opt.filter.cwd = "/fixture/else/../project".into();
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .count,
        1
    );
    opt.filter.app = Some("OUTSIDE".into());
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .results[0]
            .session
            .key
            .id,
        "outside"
    );
    opt.filter.app = None;
    opt.filter.scope = Scope::All;
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .count,
        5
    );
    opt.filter.days = Some(0);
    assert_eq!(
        search(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .count,
        0
    );
}

#[test]
fn duplicate_archive_copy_does_not_duplicate_search_or_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "one", None, None, &[event("user", "needle", None)]);
    let archive = ctx.roots.archive.as_ref().unwrap().join("copy.jsonl");
    write(&archive, &fs::read_to_string(&path).unwrap());
    let result = search(&ctx, &options("needle"), &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.count, 1);
    assert_eq!(result.results[0].matches, 1);
    assert!(!result.results[0].session.archived);
    fs::remove_file(path).unwrap();
    let result = search(&ctx, &options("needle"), &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.count, 1);
    assert!(result.results[0].session.archived);
}

fn with_coverage(ctx: &Context, coverage: Coverage) -> Context {
    let mut varied = ctx.clone().with_coverage(coverage, ctx.coverage_explicit);
    varied.index_dir = ctx.index_dir.with_extension(format!("{coverage:?}"));
    varied
}

fn found(ctx: &Context, query: &str, regex: bool) -> (u64, Vec<String>) {
    let mut opt = options(query);
    opt.regex = regex;
    let result = search(ctx, &opt, &RefreshOptions::QUERY).unwrap();
    assert!(!result.incomplete, "{:?}", result.diagnostics);
    let data = result.data.unwrap();
    data.results
        .first()
        .map_or((0, Vec::new()), |hit| (hit.matches, hit.snippets.clone()))
}

#[test]
fn compact_searches_only_tool_result_tails_on_every_path() {
    let dir = tempfile::tempdir().unwrap();
    let base = context(dir.path());
    let mut head = "early-literal Q# \0nul ".to_string();
    head.push_str(&".".repeat(8000));
    head.push_str("前日");
    let mut tail = "本 tail \0nul ".to_string();
    let end = " late-literal J@";
    tail.push_str(&".".repeat(2047 - tail.len() - end.len()));
    tail.push_str(end);
    assert_eq!(tail.len(), 2047);
    let text = head + &tail;
    assert!(text.len() > 10_000);
    claude(
        &base,
        "long",
        None,
        None,
        &[json!({"type":"user","message":{"content":[{"type":"tool_result","content":text}]}})],
    );
    let compact = with_coverage(&base, Coverage::Compact);
    let full = with_coverage(&base, Coverage::Full);
    for (query, compact_count, full_count) in [
        ("early-literal", 0, 1),
        ("late-literal", 1, 1),
        ("Q#", 0, 1),
        ("J@", 1, 1),
        ("日本", 0, 1),
        ("本 t", 1, 1),
        ("\0nul", 1, 2),
    ] {
        for regex in [false, true] {
            assert_eq!(
                found(&compact, query, regex).0,
                compact_count,
                "compact {query:?} regex={regex}"
            );
            assert_eq!(
                found(&full, query, regex).0,
                full_count,
                "full {query:?} regex={regex}"
            );
        }
    }
    assert_eq!(found(&compact, "early-lit+eral", true).0, 0);
    assert_eq!(found(&full, "early-lit+eral", true).0, 1);
    assert_eq!(found(&compact, "^本", true).0, 1);
    assert_eq!(found(&full, "^本", true).0, 0);
    assert_eq!(found(&full, "^early", true).0, 1);
    let (_, snippets) = found(&compact, "本", false);
    assert_eq!(snippets, ["[user/tool] 本 tail \0n…"]);
    let mut stored = Vec::new();
    entries(
        &compact,
        &"claude:long".parse().unwrap(),
        &RefreshOptions::QUERY,
        &mut |entry| {
            stored.push((entry.text, entry.searchable));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(stored, [(text, Searchable::ToolResult)]);
}

#[test]
fn duplicate_tool_use_result_is_stored_but_scored_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = context(dir.path());
    claude(
        &base,
        "dup",
        None,
        None,
        &[
            json!({"type":"user","toolUseResult":" dup needle\n","message":{"content":[{"type":"tool_result","content":"dup needle"}]}}),
            json!({"type":"user","toolUseResult":"other needle","message":{"content":[{"type":"tool_result","content":"dup needle"}]}}),
        ],
    );
    for coverage in [Coverage::Compact, Coverage::Full] {
        let ctx = with_coverage(&base, coverage);
        let result = search(&ctx, &options("needle"), &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap();
        assert_eq!(result.results[0].matches, 3, "{coverage:?}");
        assert_eq!(result.results[0].by_kind["tool"], 3);
        let mut stored = Vec::new();
        entries(
            &ctx,
            &"claude:dup".parse().unwrap(),
            &RefreshOptions::QUERY,
            &mut |entry| {
                stored.push((entry.kind, entry.searchable, entry.text));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            stored,
            [
                (Kind::Tool, Searchable::ToolResult, "dup needle".into()),
                (Kind::Tool, Searchable::Duplicate, " dup needle\n".into()),
                (Kind::Tool, Searchable::ToolResult, "dup needle".into()),
                (Kind::Tool, Searchable::ToolResult, "other needle".into()),
            ]
        );
    }
}
