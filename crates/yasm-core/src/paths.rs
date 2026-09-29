use camino::{Utf8Path, Utf8PathBuf};
use etcetera::BaseStrategy;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct YasmPaths {
    pub config_dir: Utf8PathBuf,
    pub data_dir: Utf8PathBuf,
    pub cache_dir: Utf8PathBuf,
}

impl YasmPaths {
    pub fn discover() -> Result<Self> {
        Self::from_user_overrides(
            env_dir("YASM_DATA_DIR")?,
            env_dir("YASM_CACHE_DIR")?,
            env_dir("YASM_CONFIG_DIR")?,
        )
    }

    fn from_user_overrides(
        data_dir: Option<Utf8PathBuf>,
        cache_dir: Option<Utf8PathBuf>,
        config_dir: Option<Utf8PathBuf>,
    ) -> Result<Self> {
        let base = etcetera::base_strategy::choose_base_strategy().map_err(|source| {
            Error::Message(format!("could not discover user directories: {source}"))
        })?;
        Ok(Self {
            data_dir: match data_dir {
                Some(path) => path,
                None => utf8_path(base.data_dir())?.join("yasm"),
            },
            cache_dir: match cache_dir {
                Some(path) => path,
                None => utf8_path(base.cache_dir())?.join("yasm"),
            },
            config_dir: match config_dir {
                Some(path) => path,
                None => utf8_path(base.config_dir())?.join("yasm"),
            },
        })
    }

    pub fn from_project_root(project_root: Utf8PathBuf) -> Result<Self> {
        let user = Self::discover()?;
        Ok(Self {
            data_dir: project_root.join(".yasm"),
            cache_dir: user.cache_dir,
            config_dir: user.config_dir,
        })
    }

    pub fn lock_file(&self) -> Utf8PathBuf {
        self.data_dir.join("yasm.lock")
    }

    pub fn skills_dir(&self) -> Utf8PathBuf {
        self.data_dir.join("skills")
    }
}

pub fn find_project_root(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let home = etcetera::home_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok());
    find_project_root_before(start, home.as_deref())
}

fn find_project_root_before(start: &Utf8Path, excluded: Option<&Utf8Path>) -> Option<Utf8PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if excluded.is_some_and(|excluded| current == excluded) {
            return None;
        }
        if current.join(".yasm").is_dir() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

pub fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    let mut out = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            camino::Utf8Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            camino::Utf8Component::CurDir => {}
            other => out.push(other.as_str()),
        }
    }
    if out.as_str().is_empty() {
        Utf8PathBuf::from(".")
    } else {
        out
    }
}

fn env_dir(name: &str) -> Result<Option<Utf8PathBuf>> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Some(Utf8PathBuf::from(value))),
        Ok(_) => Err(Error::Message(format!("{name} is set but empty"))),
        Err(_) => Ok(None),
    }
}

fn utf8_path(path: std::path::PathBuf) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path).map_err(|path| Error::NonUtf8Path(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use camino::{Utf8Path, Utf8PathBuf};

    use crate::paths::{find_project_root, find_project_root_before, normalize_path, YasmPaths};

    #[test]
    fn user_overrides_flatten_lock_and_skills_under_data() {
        let paths = YasmPaths::from_user_overrides(
            Some(Utf8PathBuf::from("/tmp/yasm-data")),
            Some(Utf8PathBuf::from("/tmp/yasm-cache")),
            Some(Utf8PathBuf::from("/tmp/yasm-config")),
        )
        .unwrap();

        assert_eq!(paths.data_dir.as_str(), "/tmp/yasm-data");
        assert_eq!(paths.cache_dir.as_str(), "/tmp/yasm-cache");
        assert_eq!(paths.config_dir.as_str(), "/tmp/yasm-config");
        assert_eq!(paths.lock_file().as_str(), "/tmp/yasm-data/yasm.lock");
        assert_eq!(paths.skills_dir().as_str(), "/tmp/yasm-data/skills");
    }

    #[test]
    fn find_project_root_walks_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".yasm")).unwrap();

        assert_eq!(find_project_root(&nested).as_deref(), Some(root.as_path()));
        assert_eq!(find_project_root(&root).as_deref(), Some(root.as_path()));
    }

    #[test]
    fn find_project_root_does_not_select_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let nested = home.join("projects/demo");
        std::fs::create_dir_all(home.join(".yasm")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root_before(&nested, Some(&home)), None);
    }

    #[test]
    fn normalize_path_resolves_parent_dirs() {
        assert_eq!(
            normalize_path(Utf8Path::new("/repo/.agents/skills/../../.yasm/skills/x")).as_str(),
            "/repo/.yasm/skills/x"
        );
    }
}
