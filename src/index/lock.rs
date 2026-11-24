use crate::{Context, Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub(super) fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}

pub(super) fn canonical(path: &Path, cwd: &Path) -> Result<PathBuf> {
    canonical_path(path, cwd, false)
}

fn source_identity(path: &Path, cwd: &Path) -> Result<PathBuf> {
    canonical_path(path, cwd, true)
}

fn canonical_path(path: &Path, cwd: &Path, source: bool) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            component => {
                resolved.push(component);
                match fs::canonicalize(&resolved) {
                    Ok(path) => resolved = path,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::NotFound
                            || source
                                && matches!(
                                    e.kind(),
                                    std::io::ErrorKind::PermissionDenied
                                        | std::io::ErrorKind::NotADirectory
                                ) => {}
                    Err(e) => return Err(io(&resolved, e)),
                }
            }
        }
    }
    Ok(resolved)
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

pub(super) fn namespace(ctx: &Context) -> Result<(String, String)> {
    let mut roots = serde_json::to_value(&ctx.roots)?;
    for value in roots
        .as_object_mut()
        .expect("Roots is an object")
        .values_mut()
    {
        if let Some(path) = value.as_str() {
            *value = serde_json::to_value(source_identity(Path::new(path), &ctx.cwd)?)?;
        }
    }
    let encoded = serde_json::to_string(&roots)?;
    Ok((hex_digest(encoded.as_bytes()), encoded))
}

pub(super) fn check_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.is_file()
                && meta.nlink() == 1
                && meta.permissions().mode() & 0o777 == 0o600 =>
        {
            Ok(true)
        }
        Ok(_) => Err(Error::InvalidArgument(
            "index files must be private regular files with mode 0600".into(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io(path, e)),
    }
}

pub(super) fn directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && meta.permissions().mode() & 0o777 == 0o700 => Ok(()),
        Ok(_) => Err(Error::InvalidArgument(
            "index directories must be private directories with mode 0700".into(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && !parent.exists()
            {
                directory(parent)?;
            }
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => directory(path),
                Err(e) => Err(io(path, e)),
            }
        }
        Err(e) => Err(io(path, e)),
    }
}

pub(super) fn validate(ctx: &Context) -> Result<PathBuf> {
    if !ctx.cwd.is_absolute() {
        return Err(Error::InvalidArgument(
            "Context.cwd must be absolute".into(),
        ));
    }
    let lexical = if ctx.index_dir.is_absolute() {
        ctx.index_dir.clone()
    } else {
        ctx.cwd.join(&ctx.index_dir)
    };
    if fs::symlink_metadata(&lexical).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::InvalidArgument(
            "index directory cannot be a symlink".into(),
        ));
    }
    let index = canonical(&lexical, &ctx.cwd)?;
    let roots = serde_json::to_value(&ctx.roots)?;
    for (field, value) in roots.as_object().expect("Roots is an object") {
        if let Some(path) = value.as_str() {
            let root = source_identity(Path::new(path), &ctx.cwd)?;
            let root = if matches!(
                field.as_str(),
                "opencode_db" | "codex_session_index" | "devin_sessions_db"
            ) {
                root.parent().unwrap_or(&root)
            } else {
                &root
            };
            if index.starts_with(root) || root.starts_with(&index) {
                return Err(Error::InvalidArgument(
                    "index directory overlaps a native source root".into(),
                ));
            }
        }
    }
    directory(&index)?;
    Ok(index)
}

fn lock_file(path: &Path) -> Result<File> {
    let existed = check_file(path)?;
    if existed {
        OpenOptions::new().read(true).write(true).open(path)
    } else {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                check_file(path)?;
                OpenOptions::new().read(true).write(true).open(path)
            }
            result => result,
        }
    }
    .map_err(|e| io(path, e))
}

enum Mode {
    Exclusive,
    Shared,
}

fn poll(file: &File, path: &Path, mode: &Mode, timeout: Duration) -> Result<bool> {
    let started = Instant::now();
    loop {
        let attempt = match mode {
            Mode::Exclusive => file.try_lock(),
            Mode::Shared => file.try_lock_shared(),
        };
        match attempt {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < timeout => {
                std::thread::sleep(
                    Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
                );
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(std::fs::TryLockError::Error(e)) => return Err(io(path, e)),
        }
    }
}

pub(super) struct Lease {
    _file: File,
}

impl Lease {
    pub(super) fn acquire(
        directory: &Path,
        timeout: Duration,
        alone: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        let path = directory.join("lifecycle.lock");
        let file = lock_file(&path)?;
        if poll(&file, &path, &Mode::Exclusive, Duration::ZERO)? {
            let cleaned = alone();
            file.unlock().map_err(|e| io(&path, e))?;
            cleaned?;
        }
        if poll(&file, &path, &Mode::Shared, timeout)? {
            Ok(Self { _file: file })
        } else {
            Err(Error::IndexBusy)
        }
    }
}

pub(super) fn writer(directory: &Path, timeout: Duration, notice: bool) -> Result<Option<File>> {
    let path = directory.join("writer.lock");
    let file = lock_file(&path)?;
    if poll(&file, &path, &Mode::Exclusive, Duration::ZERO)? {
        return Ok(Some(file));
    }
    if notice {
        eprintln!("waiting for another slimetrail refresh…");
    }
    Ok(poll(&file, &path, &Mode::Exclusive, timeout)?.then_some(file))
}

fn private_entry(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if !meta.is_dir() || meta.permissions().mode() & 0o777 != 0o700 {
        return Err(Error::InvalidArgument(
            "unsafe index snapshot staging entry".into(),
        ));
    }
    Ok(())
}

fn remove_tree(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(path, e)),
        _ => Ok(()),
    }
}

fn entries(staging: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(staging).map_err(|e| io(staging, e))? {
        let entry = entry.map_err(|e| io(staging, e))?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            found.push(entry.path());
        }
    }
    Ok(found)
}

pub(super) fn cleanup(staging: &Path) -> Result<()> {
    directory(staging)?;
    for prefix in ["slimetrail-snapshot-", "tmp-"] {
        for path in entries(staging, prefix)? {
            private_entry(&path)?;
            remove_tree(&path)?;
        }
    }
    Ok(())
}

pub(super) fn sweep(staging: &Path, own: &Path) -> Result<()> {
    sweep_listed(entries(staging, "proc-")?, own)
}

fn sweep_listed(listed: Vec<PathBuf>, own: &Path) -> Result<()> {
    for path in listed {
        if path == own {
            continue;
        }
        match private_entry(&path) {
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            other => other?,
        }
        let owner = path.join(OWNER);
        let file = match check_file(&owner) {
            Ok(true) => match OpenOptions::new().read(true).write(true).open(&owner) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io(&owner, e)),
            },
            Ok(false) => continue,
            Err(e) => return Err(e),
        };
        if poll(&file, &owner, &Mode::Exclusive, Duration::ZERO)? {
            remove_tree(&path)?;
        }
    }
    Ok(())
}

const OWNER: &str = "owner.lock";

fn random() -> String {
    use std::hash::BuildHasher;
    let seed = (
        std::process::id(),
        Instant::now(),
        std::thread::current().id(),
    );
    format!(
        "{:016x}",
        std::collections::hash_map::RandomState::new().hash_one(format!("{seed:?}"))
    )
}

pub(super) struct Staging {
    pub(super) path: PathBuf,
    _owner: File,
}

impl Staging {
    pub(super) fn create(staging: &Path) -> Result<Self> {
        directory(staging)?;
        let temporary = staging.join(format!("tmp-{}", random()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&temporary)
            .map_err(|e| io(&temporary, e))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
            .map_err(|e| io(&temporary, e))?;
        let owner = temporary.join(OWNER);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&owner)
            .map_err(|e| io(&owner, e))?;
        if !poll(&file, &owner, &Mode::Exclusive, Duration::ZERO)? {
            return Err(Error::InvalidArgument(
                "new snapshot staging owner lock is already held".into(),
            ));
        }
        let path = staging.join(format!("proc-{}-{}", std::process::id(), random()));
        fs::rename(&temporary, &path).map_err(|e| io(&path, e))?;
        Ok(Self { path, _owner: file })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return;
        };
        let only_owner = entries
            .filter_map(std::result::Result::ok)
            .all(|entry| entry.file_name() == OWNER);
        if only_owner {
            let _ = fs::remove_file(self.path.join(OWNER));
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hex_digest, sweep_listed};
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn sweep_skips_a_listed_directory_that_vanished() {
        let root = std::env::temp_dir().join(format!("sweep-vanish-{}", super::random()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let gone = root.join("proc-1-gone");
        let dead = root.join("proc-2-dead");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dead)
            .unwrap();
        std::fs::write(dead.join("owner.lock"), b"").unwrap();
        std::fs::set_permissions(
            dead.join("owner.lock"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        let own = root.join("proc-3-own");
        sweep_listed(vec![gone, dead.clone()], &own).unwrap();
        assert!(!dead.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hex_digest_is_lowercase_sha256_hex() {
        assert_eq!(
            hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
