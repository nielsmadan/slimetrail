use super::{Source, jsonl::FileReader};
use crate::{Agent, Context, Discovery, DiscoveryCache, Entry, ParsedSession, RawSession, Result};

pub struct Droid(FileReader);

impl Droid {
    pub fn new(ctx: &Context) -> Self {
        Self(FileReader::new(Agent::Droid, ctx))
    }
}

impl Source for Droid {
    fn agent(&self) -> Agent {
        Agent::Droid
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
}
