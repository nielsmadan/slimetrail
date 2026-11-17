use slimetrail::{Roots, Session, SessionKey, TitleOrigin, resolve_roots};
use std::{collections::BTreeMap, path::Path};

pub fn roots_at(base: &Path) -> Roots {
    let env = BTreeMap::from([
        (
            "CODEX_HOME".into(),
            base.join("codex").to_string_lossy().into_owned(),
        ),
        (
            "XDG_DATA_HOME".into(),
            base.join("data").to_string_lossy().into_owned(),
        ),
    ]);
    resolve_roots(base, &env).unwrap()
}

pub fn session() -> Session {
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
