use crate::{
    Kind, ReadMode, ReadOptions, ReadPage, Result, Session, dto::format_timestamp,
    index::IndexSession,
};

pub(crate) fn run(
    index: &IndexSession,
    session: Session,
    options: &ReadOptions,
) -> Result<ReadPage> {
    let mut page = ReadPage {
        session,
        mode: options.mode,
        text: String::new(),
        offset: options.offset,
        next_offset: None,
        total_chars: 0,
        returned_chars: 0,
        truncated: false,
    };
    index.visit_entries(&page.session.key.clone(), &mut |entry| {
        if entry.kind == Kind::Tool && options.mode != ReadMode::Log
            || entry.kind == Kind::Meta && options.mode == ReadMode::Lite
        {
            return Ok(());
        }
        let timestamp = match entry.timestamp_raw.as_deref().filter(|s| !s.is_empty()) {
            Some(raw) => Some(raw.to_string()),
            None => entry.timestamp.map(format_timestamp).transpose()?,
        };
        let heading = format!(
            "{}[{}/{}]\n",
            timestamp.map(|s| format!("{s} ")).unwrap_or_default(),
            entry.role,
            kind_name(entry.kind)
        );
        for part in [
            heading.as_str(),
            entry.text.trim_end_matches(python_whitespace),
            "\n\n",
        ] {
            let count = part.chars().count();
            let start = options.offset.saturating_sub(page.total_chars);
            let take = options.max_chars - page.returned_chars;
            if start < count && take > 0 {
                let appended = (count - start).min(take);
                page.text.extend(part.chars().skip(start).take(appended));
                page.returned_chars += appended;
            }
            page.total_chars += count;
        }
        Ok(())
    })?;
    let end = page.offset.saturating_add(page.returned_chars);
    page.truncated = end < page.total_chars;
    page.next_offset = page.truncated.then_some(end);
    Ok(page)
}

pub(crate) const fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Message => "message",
        Kind::Tool => "tool",
        Kind::Meta => "meta",
    }
}

pub(crate) fn python_whitespace(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}
