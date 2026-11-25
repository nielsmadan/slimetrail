use crate::{Coverage, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub coverage: Coverage,
    pub explicit: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    coverage: Option<Coverage>,
}

pub fn path(home: &Path, env: &BTreeMap<String, String>) -> PathBuf {
    let value = |name: &str| env.get(name).filter(|s| !s.is_empty()).map(PathBuf::from);
    value("SLIMETRAIL_CONFIG").unwrap_or_else(|| {
        value("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("slimetrail/config.toml")
    })
}

fn invalid(path: &Path, reason: &dyn std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("invalid config file {}: {reason}", path.display()))
}

pub fn load(path: &Path) -> Result<Config> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(invalid(path, &"not UTF-8"));
        }
        Err(source) => {
            return Err(Error::Io {
                path: path.into(),
                source,
            });
        }
    };
    let stored: Stored = toml::from_str(&text).map_err(|e| invalid(path, &e.message()))?;
    Ok(Config {
        coverage: stored.coverage.unwrap_or_default(),
        explicit: stored.coverage.is_some(),
    })
}

pub fn write_coverage(path: &Path, coverage: Coverage) -> Result<()> {
    let io = |source| Error::Io {
        path: path.into(),
        source,
    };
    let target = if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::canonicalize(path).map_err(io)?
    } else {
        path.to_path_buf()
    };
    let mode = fs::metadata(&target).map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(io)?;
    let text = toml::to_string(&Stored {
        coverage: Some(coverage),
    })
    .map_err(|e| invalid(path, &e))?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
    file.write_all(text.as_bytes()).map_err(io)?;
    fs::set_permissions(file.path(), fs::Permissions::from_mode(mode)).map_err(io)?;
    file.persist(&target).map_err(|e| io(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefers_explicit_file_then_xdg_then_home() {
        let home = Path::new("/fixture/home");
        let mut env = BTreeMap::new();
        assert_eq!(
            path(home, &env),
            home.join(".config/slimetrail/config.toml")
        );
        env.insert("XDG_CONFIG_HOME".into(), "/fixture/xdg".into());
        assert_eq!(
            path(home, &env),
            Path::new("/fixture/xdg/slimetrail/config.toml")
        );
        env.insert("SLIMETRAIL_CONFIG".into(), "/fixture/other.toml".into());
        assert_eq!(path(home, &env), Path::new("/fixture/other.toml"));
        env.insert("SLIMETRAIL_CONFIG".into(), String::new());
        assert_eq!(
            path(home, &env),
            Path::new("/fixture/xdg/slimetrail/config.toml")
        );
    }

    #[test]
    fn load_defaults_validates_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested/config.toml");
        assert_eq!(
            load(&file).unwrap(),
            Config {
                coverage: Coverage::Compact,
                explicit: false
            }
        );
        write_coverage(&file, Coverage::Full).unwrap();
        assert_eq!(
            load(&file).unwrap(),
            Config {
                coverage: Coverage::Full,
                explicit: true
            }
        );
        fs::write(&file, "# no keys\n").unwrap();
        assert_eq!(load(&file).unwrap(), Config::default());
        fs::write(&file, "coverage = \"compact\"\n").unwrap();
        assert_eq!(
            load(&file).unwrap(),
            Config {
                coverage: Coverage::Compact,
                explicit: true
            }
        );
        for bad in [
            "coverage = \"everything\"\n",
            "coverage = 1\n",
            "other = true\n",
            "coverage = ",
        ] {
            fs::write(&file, bad).unwrap();
            let message = load(&file).unwrap_err().to_string();
            assert!(message.contains(&file.display().to_string()), "{message}");
        }
    }

    #[test]
    fn write_coverage_follows_symlinks_keeps_mode_and_defaults_to_0644() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/real.toml");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "# note\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("config.toml");
        symlink(&target, &link).unwrap();
        write_coverage(&link, Coverage::Full).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "coverage = \"full\"\n"
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let fresh = dir.path().join("new/config.toml");
        write_coverage(&fresh, Coverage::Compact).unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let dangling = dir.path().join("dangling.toml");
        symlink(dir.path().join("missing/none.toml"), &dangling).unwrap();
        let message = write_coverage(&dangling, Coverage::Full)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(&dangling.display().to_string()),
            "{message}"
        );
    }
}
