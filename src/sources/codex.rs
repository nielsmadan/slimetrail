use super::{
    Source,
    jsonl::{self, FileReader},
};
use crate::{Agent, Context, Discovery, DiscoveryCache, Entry, ParsedSession, RawSession, Result};
use std::path::Path;

pub struct Codex(FileReader);

impl Codex {
    pub fn new(ctx: &Context) -> Self {
        Self(FileReader::new(Agent::Codex, ctx))
    }
}

impl Source for Codex {
    fn agent(&self) -> Agent {
        Agent::Codex
    }
    fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
        self.0.discover(ctx, previous)
    }
    fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession> {
        self.0.parse(raw, emit)
    }
    fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
        jsonl::transcript(raw)
    }
    fn drifted(&self, raw: &RawSession) -> bool {
        jsonl::drifted(raw)
    }
}
