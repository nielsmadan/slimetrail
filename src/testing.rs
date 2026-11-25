use crate::{Context, resolve_roots};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(crate) struct ContextBuilder {
    base: PathBuf,
    index_dir: PathBuf,
}

pub(crate) fn context_at(base: &Path) -> ContextBuilder {
    ContextBuilder {
        base: base.into(),
        index_dir: base.join("index"),
    }
}

pub(crate) fn context(base: &Path) -> Context {
    context_at(base).build()
}

impl ContextBuilder {
    pub(crate) fn index_dir(self, index_dir: PathBuf) -> Self {
        Self { index_dir, ..self }
    }

    pub(crate) fn build(self) -> Context {
        Context::new(
            resolve_roots(&self.base, &BTreeMap::new()).unwrap(),
            self.index_dir,
            self.base,
            0,
            "UTC".into(),
        )
    }
}
