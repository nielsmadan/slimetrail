#[allow(dead_code)]
mod query_support;
use query_support::*;
use slimetrail::*;

#[test]
fn scalar_pages_concatenate_across_timestamps_headers_and_multibyte_text() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[
            event(
                "user",
                "α🐌e\u{301}\nline \t\u{1f}",
                Some("2026-01-02T03:04:05+03:00"),
            ),
            event("assistant", "完了", None),
        ],
    );
    let key = "claude:one".parse().unwrap();
    let expected = "2026-01-02T03:04:05+03:00 [user/message]\nα🐌e\u{301}\nline\n\n[assistant/message]\n完了\n\n";
    let full = read(
        &ctx,
        &key,
        &ReadOptions {
            mode: ReadMode::Log,
            offset: 0,
            max_chars: 1000,
        },
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(full.text, expected);
    assert_eq!(full.total_chars, expected.chars().count());
    for size in [1, 2, 7, 37, 100] {
        let mut combined = String::new();
        let mut offset = 0;
        loop {
            let page = read(
                &ctx,
                &key,
                &ReadOptions {
                    mode: ReadMode::Log,
                    offset,
                    max_chars: size,
                },
            )
            .unwrap()
            .data
            .unwrap();
            assert_eq!(page.returned_chars, page.text.chars().count());
            assert_eq!(page.total_chars, full.total_chars);
            combined.push_str(&page.text);
            match page.next_offset {
                Some(next) => {
                    assert_eq!(next, offset + page.returned_chars);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(combined, expected);
    }
}

#[test]
fn beyond_end_is_empty_and_zero_limit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", None, None, &[event("user", "text", None)]);
    let key = "claude:one".parse().unwrap();
    let page = read(
        &ctx,
        &key,
        &ReadOptions {
            mode: ReadMode::Lite,
            offset: usize::MAX,
            max_chars: 3,
        },
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(page.text, "");
    assert_eq!(page.returned_chars, 0);
    assert!(!page.truncated);
    assert_eq!(page.next_offset, None);
    assert!(
        read(
            &ctx,
            &key,
            &ReadOptions {
                mode: ReadMode::Lite,
                offset: 0,
                max_chars: 0
            }
        )
        .is_err()
    );
}
