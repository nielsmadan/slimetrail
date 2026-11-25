pub mod claude;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod droid;
pub mod jsonl;
pub mod opencode;
pub mod pi;
pub mod sqlite;
mod state;

pub use state::*;

use crate::{Agent, Context, Entry, Result};
use std::path::Path;

pub(crate) fn derive_title(
    session: &mut crate::Session,
    role: &str,
    kind: crate::Kind,
    text: &str,
) {
    if session.title.is_none() && role == "user" && kind == crate::Kind::Message {
        session.title = Some(
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(120)
                .collect(),
        );
        session.title_origin = Some(crate::TitleOrigin::Derived);
    }
}

pub trait Source {
    fn agent(&self) -> Agent;
    fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery>;
    fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession>;
    fn transcript<'a>(&self, _raw: &'a RawSession) -> Option<&'a Path> {
        None
    }
    fn drifted(&self, _raw: &RawSession) -> bool {
        false
    }
}

pub fn registry(ctx: &Context) -> Vec<Box<dyn Source>> {
    vec![
        Box::new(claude::Claude::new(ctx)),
        Box::new(codex::Codex::new(ctx)),
        Box::new(cursor::Cursor::new(ctx)),
        Box::new(devin::Devin::new(ctx)),
        Box::new(droid::Droid::new(ctx)),
        Box::new(pi::Pi::new(ctx)),
        Box::new(opencode::Opencode::new(ctx)),
    ]
}

pub fn source_for(ctx: &Context, agent: Agent) -> Option<Box<dyn Source>> {
    registry(ctx)
        .into_iter()
        .find(|source| source.agent() == agent)
}

pub(crate) fn registry_in(ctx: &Context, staging: &std::path::Path) -> Vec<Box<dyn Source>> {
    vec![
        Box::new(claude::Claude::new(ctx)),
        Box::new(codex::Codex::new(ctx)),
        Box::new(cursor::Cursor::with_staging(ctx, staging)),
        Box::new(devin::Devin::with_staging(staging)),
        Box::new(droid::Droid::new(ctx)),
        Box::new(pi::Pi::new(ctx)),
        Box::new(opencode::Opencode::with_staging(staging)),
    ]
}
