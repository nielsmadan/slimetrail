use crate::{Context, Error, Result};
use sha2::{Digest, Sha256};
use std::{
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
    Ok((format!("{:x}", Sha256::digest(encoded.as_bytes())), encoded))
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

pub(super) struct Lease {
    _file: File,
}

impl Lease {
    pub(super) fn acquire(directory: &Path, timeout: Duration) -> Result<Self> {
        let path = directory.join("lifecycle.lock");
        let existed = check_file(&path)?;
        let file = if existed {
            OpenOptions::new().read(true).write(true).open(&path)
        } else {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    check_file(&path)?;
                    OpenOptions::new().read(true).write(true).open(&path)
                }
                result => result,
            }
        }
        .map_err(|e| io(&path, e))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < timeout => {
                    std::thread::sleep(
                        Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
                    );
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(Error::IndexBusy),
                Err(std::fs::TryLockError::Error(e)) => return Err(io(&path, e)),
            }
        }
    }
}

pub(super) fn cleanup(staging: &Path) -> Result<()> {
    directory(staging)?;
    for entry in fs::read_dir(staging).map_err(|e| io(staging, e))? {
        let entry = entry.map_err(|e| io(staging, e))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("slimetrail-snapshot-")
        {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
            if !meta.is_dir() || meta.permissions().mode() & 0o777 != 0o700 {
                return Err(Error::InvalidArgument(
                    "unsafe index snapshot staging entry".into(),
                ));
            }
            fs::remove_dir_all(&path).map_err(|e| io(&path, e))?;
        }
    }
    Ok(())
}
