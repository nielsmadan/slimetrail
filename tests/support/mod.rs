use slimetrail::{Context, Coverage, Roots, Session, SessionKey, TitleOrigin, resolve_roots};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

fn env(base: &Path, codex_home: bool, data_home: bool) -> BTreeMap<String, String> {
    [
        (codex_home, "CODEX_HOME", "codex"),
        (data_home, "XDG_DATA_HOME", "data"),
    ]
    .into_iter()
    .filter(|(set, _, _)| *set)
    .map(|(_, name, dir)| (name.into(), base.join(dir).to_string_lossy().into_owned()))
    .collect()
}

pub(crate) fn roots_at(base: &Path) -> Roots {
    resolve_roots(base, &env(base, true, true)).unwrap()
}

pub(crate) struct ContextBuilder {
    base: PathBuf,
    codex_home: bool,
    data_home: bool,
    cwd: PathBuf,
    now_ms: i64,
    coverage: Coverage,
}

pub(crate) fn context_at(base: &Path) -> ContextBuilder {
    ContextBuilder {
        base: base.into(),
        codex_home: true,
        data_home: true,
        cwd: "/fixture/project".into(),
        now_ms: 0,
        coverage: Coverage::Compact,
    }
}

pub(crate) fn context(base: &Path) -> Context {
    context_at(base).build()
}

impl ContextBuilder {
    pub(crate) fn without_codex_home(self) -> Self {
        Self {
            codex_home: false,
            ..self
        }
    }

    pub(crate) fn without_data_home(self) -> Self {
        Self {
            data_home: false,
            ..self
        }
    }

    pub(crate) fn cwd(self, cwd: &Path) -> Self {
        Self {
            cwd: cwd.into(),
            ..self
        }
    }

    pub(crate) fn now_ms(self, now_ms: i64) -> Self {
        Self { now_ms, ..self }
    }

    pub(crate) fn coverage(self, coverage: Coverage) -> Self {
        Self { coverage, ..self }
    }

    pub(crate) fn build(self) -> Context {
        Context::new(
            resolve_roots(
                &self.base,
                &env(&self.base, self.codex_home, self.data_home),
            )
            .unwrap(),
            self.base.join("index"),
            self.cwd,
            self.now_ms,
            "UTC".into(),
        )
        .with_coverage(self.coverage, false)
    }
}

pub(crate) fn an_hour_later(ctx: &Context, path: &Path) -> Context {
    let written = fs::metadata(path).unwrap();
    let mut later = ctx.clone();
    later.now_ms = written.mtime() * 1000 + written.mtime_nsec() / 1_000_000 + 3_600_000;
    later
}

pub(crate) fn session() -> Session {
    Session {
        key: "codex:synthetic".parse::<SessionKey>().unwrap(),
        title: Some("Synthetic session".into()),
        title_origin: Some(TitleOrigin::Native),
        source_updated_at: Some(1000),
        cwd: Some("/fixture/project".into()),
        branch: Some("main".into()),
        started_at: Some(0),
        updated_at: Some(1000),
        parent: None,
        source: "fixture.jsonl".into(),
        archived: false,
    }
}
