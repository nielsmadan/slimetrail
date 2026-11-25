pub mod config;
pub mod dto;
pub mod error;
pub mod index;
mod model;
pub mod roots;

pub use error::{Error, Result};
pub use model::*;
pub use roots::resolve_roots;
pub mod api;
mod hex;
mod read;
mod search;
pub mod sources;
#[cfg(test)]
mod testing;
mod timeline;
pub use api::{entries, read, refresh, search, sessions, timeline};
pub(crate) use sources::{
    AppendState, CachedSource, Discovery, DiscoveryCache, Fingerprint, ParsedSession,
    PrivateSnapshot, RawSession, SourceState,
};
