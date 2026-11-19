use slimetrail::{Envelope, dto::SearchDataDto};
use std::{
    fmt::{self, Write as _},
    io::{self, Write},
};

pub(super) fn render(out: &mut dyn Write, result: &Envelope<SearchDataDto>) -> io::Result<()> {
    let hits = result
        .data
        .as_ref()
        .map_or(&[][..], |data| data.results.as_slice());
    if hits.is_empty() {
        return writeln!(
            out,
            "{}",
            if result.incomplete {
                "No matches in available sources (search incomplete)."
            } else {
                "No matches."
            }
        );
    }
    writeln!(
        out,
        "Returned sessions: {}{}",
        hits.len(),
        if result.incomplete {
            " (search incomplete)"
        } else {
            ""
        }
    )?;
    for hit in hits {
        writeln!(out, "\nRef: {}", DisplayText(&hit.session.r#ref))?;
        for (label, value) in [
            ("Title", hit.session.title.as_deref()),
            ("Cwd", hit.session.cwd.as_deref()),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                writeln!(out, "{label}: {}", DisplayText(value))?;
            }
        }
        writeln!(out, "Occurrences: {} | Score: {}", hit.matches, hit.score)?;
        for snippet in &hit.snippets {
            writeln!(out, "  {}", DisplayText(snippet))?;
        }
    }
    Ok(())
}

struct DisplayText<'a>(&'a str);

impl fmt::Display for DisplayText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.chars().try_for_each(|ch| {
            if ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                write!(f, "{}", ch.escape_default())
            } else {
                f.write_char(ch)
            }
        })
    }
}
