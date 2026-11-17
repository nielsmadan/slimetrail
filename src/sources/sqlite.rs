use super::jsonl::fingerprint;
use crate::{Error, Fingerprint, PrivateSnapshot, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn dependencies(database: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .map(|suffix| {
            let mut path = database.as_os_str().to_owned();
            path.push(suffix);
            PathBuf::from(path)
        })
        .into()
}

fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}

fn stamp(meta: &fs::Metadata) -> [i128; 7] {
    [
        meta.dev().into(),
        meta.ino().into(),
        meta.len().into(),
        meta.mtime().into(),
        meta.mtime_nsec().into(),
        meta.ctime().into(),
        meta.ctime_nsec().into(),
    ]
}

fn regular(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(m)),
        Ok(_) => Err(Error::InvalidArgument(
            "SQLite dependency is not a regular file".into(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

struct Captured {
    path: PathBuf,
    file: Option<File>,
    stamp: Option<[i128; 7]>,
}

fn capture(path: &Path) -> Result<Captured> {
    let before = regular(path)?.map(|m| stamp(&m));
    let file = if before.is_some() {
        let file = File::open(path).map_err(|e| io(path, e))?;
        let meta = file.metadata().map_err(|e| io(path, e))?;
        if !meta.is_file() || Some(stamp(&meta)) != before {
            return Err(Error::InvalidArgument(
                "SQLite source changed during capture".into(),
            ));
        }
        Some(file)
    } else {
        None
    };
    Ok(Captured {
        path: path.into(),
        file,
        stamp: before,
    })
}

fn unchanged(captured: &Captured) -> Result<bool> {
    let path = regular(&captured.path)?.map(|m| stamp(&m));
    let handle = captured
        .file
        .as_ref()
        .map(|f| f.metadata().map(|m| stamp(&m)))
        .transpose()
        .map_err(|e| io(&captured.path, e))?;
    Ok(path == captured.stamp && handle == captured.stamp)
}

fn copy_captured(
    reader: impl Read,
    writer: &mut impl Write,
    length: u64,
    target: &Path,
) -> Result<()> {
    let count = std::io::copy(&mut reader.take(length), writer).map_err(|e| io(target, e))?;
    writer.flush().map_err(|e| io(target, e))?;
    if count != length {
        return Err(Error::InvalidArgument(
            "SQLite source changed during copy".into(),
        ));
    }
    Ok(())
}

fn attempt(
    database: &Path,
    staging: Option<&Path>,
    copied: &mut dyn FnMut() -> Result<()>,
) -> Result<(Arc<PrivateSnapshot>, Fingerprint)> {
    let paths = dependencies(database);
    let before = fingerprint(&paths)?;
    let captured = paths
        .iter()
        .map(|p| capture(p))
        .collect::<Result<Vec<_>>>()?;
    if captured[0].file.is_none() {
        return Err(Error::InvalidArgument("SQLite database is missing".into()));
    }
    if captured[3].stamp.is_some_and(|s| s[2] != 0) {
        return Err(Error::InvalidArgument(
            "SQLite rollback journal requires recovery".into(),
        ));
    }
    if fingerprint(&paths)? != before {
        return Err(Error::InvalidArgument(
            "SQLite source changed during capture".into(),
        ));
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("slimetrail-snapshot-");
    let directory = match staging {
        Some(staging) => builder.tempdir_in(staging),
        None => builder.tempdir(),
    }
    .map_err(|e| io(database, e))?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .map_err(|e| io(directory.path(), e))?;
    let private_db = directory.path().join("snapshot.db");
    let destinations = dependencies(&private_db);
    for (source, target) in captured.iter().zip(&destinations).take(2) {
        if let (Some(file), Some(meta)) = (&source.file, source.stamp) {
            let mut writer = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(target)
                .map_err(|e| io(target, e))?;
            let length = u64::try_from(meta[2])
                .map_err(|_| Error::InvalidArgument("SQLite file is too large".into()))?;
            copy_captured(file, &mut writer, length, target)?;
        }
    }
    copied()?;
    let after = captured.iter().map(unchanged).collect::<Result<Vec<_>>>()?;
    if after.iter().any(|same| !same) || fingerprint(&paths)? != before {
        return Err(Error::InvalidArgument(
            "SQLite source changed during copy".into(),
        ));
    }
    Ok((
        Arc::new(PrivateSnapshot {
            directory,
            database: private_db,
        }),
        before,
    ))
}

fn snapshot_with(
    database: &Path,
    copied: &mut dyn FnMut() -> Result<()>,
) -> Result<(Arc<PrivateSnapshot>, Fingerprint)> {
    attempt(database, None, copied).or_else(|_| attempt(database, None, copied))
}

pub fn snapshot(database: &Path) -> Result<(Arc<PrivateSnapshot>, Fingerprint)> {
    snapshot_with(database, &mut || Ok(()))
}

pub fn snapshot_in(database: &Path, staging: &Path) -> Result<(Arc<PrivateSnapshot>, Fingerprint)> {
    let metadata = fs::symlink_metadata(staging).map_err(|e| io(staging, e))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(Error::InvalidArgument(
            "snapshot staging must be a private directory".into(),
        ));
    }
    attempt(database, Some(staging), &mut || Ok(()))
        .or_else(|_| attempt(database, Some(staging), &mut || Ok(())))
}

pub(crate) fn snapshot_at(
    database: &Path,
    staging: Option<&Path>,
) -> Result<(Arc<PrivateSnapshot>, Fingerprint)> {
    match staging {
        Some(staging) => snapshot_in(database, staging),
        None => snapshot(database),
    }
}

pub fn open(snapshot: &PrivateSnapshot) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        &snapshot.database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_millis(100))?;
    connection.execute_batch(
        "PRAGMA trusted_schema=OFF; SELECT count(*) FROM sqlite_schema; PRAGMA query_only=ON;",
    )?;
    for path in dependencies(&snapshot.database) {
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|e| io(&path, e))?;
        }
    }
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ChangingReader<'a> {
        file: &'a File,
        change: Option<Box<dyn FnOnce()>>,
    }

    impl Read for ChangingReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let length = buffer.len().min(2);
            let count = self.file.read(&mut buffer[..length])?;
            if let Some(change) = self.change.take() {
                change();
            }
            Ok(count)
        }
    }

    #[test]
    fn during_copy_append_is_bounded_and_truncation_is_rejected() {
        for append in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.db");
            fs::write(&path, b"original").unwrap();
            let captured = capture(&path).unwrap();
            let changed_path = path.clone();
            let reader = ChangingReader {
                file: captured.file.as_ref().unwrap(),
                change: Some(Box::new(move || {
                    let mut writer = OpenOptions::new()
                        .write(true)
                        .append(append)
                        .open(changed_path)
                        .unwrap();
                    if append {
                        writer.write_all(b"appended").unwrap();
                    } else {
                        writer.set_len(2).unwrap();
                    }
                })),
            };
            let mut output = Vec::new();
            let result = copy_captured(reader, &mut output, 8, &dir.path().join("private.db"));
            if append {
                result.unwrap();
                assert_eq!(output, b"original");
                assert_eq!(fs::read(&path).unwrap(), b"originalappended");
            } else {
                assert!(
                    matches!(result, Err(Error::InvalidArgument(message)) if message == "SQLite source changed during copy")
                );
                assert_eq!(output, b"or");
            }
            assert!(!unchanged(&captured).unwrap());
        }
    }

    #[test]
    fn churn_retries_once_and_rejects_second_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE events(value); INSERT INTO events VALUES(1);",
        )
        .unwrap();
        let mut calls = 0;
        let result = snapshot_with(&path, &mut || {
            calls += 1;
            db.execute("INSERT INTO events VALUES(?)", [calls])?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_single_change_retries_with_new_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE events(value); INSERT INTO events VALUES(1);",
        )
        .unwrap();
        let mut calls = 0;
        let (copy, _) = snapshot_with(&path, &mut || {
            calls += 1;
            if calls == 1 {
                db.execute("INSERT INTO events VALUES(2)", [])?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            open(&copy)
                .unwrap()
                .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn journal_appearance_and_replacement_invalidate_capture() {
        for change in ["journal", "replacement", "shm"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.db");
            Connection::open(&path)
                .unwrap()
                .execute_batch("CREATE TABLE events(value);")
                .unwrap();
            let result = attempt(&path, None, &mut || {
                match change {
                    "journal" => fs::write(dependencies(&path)[3].clone(), b"journal").unwrap(),
                    "shm" => fs::write(dependencies(&path)[2].clone(), b"shared").unwrap(),
                    _ => {
                        let other = dir.path().join("replacement.db");
                        fs::copy(&path, &other).unwrap();
                        fs::rename(other, &path).unwrap();
                    }
                }
                Ok(())
            });
            assert!(result.is_err(), "{change}");
        }
    }
}
