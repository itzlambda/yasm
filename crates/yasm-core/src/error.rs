use camino::Utf8PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("invalid {kind}: {value}")]
    InvalidValue { kind: &'static str, value: String },
    #[error("non-UTF-8 path is not supported: {0}")]
    NonUtf8Path(String),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{action}: failed to {operation}: {source}")]
    SubprocessIo {
        action: String,
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON error at {path}: {source}")]
    Json {
        path: Utf8PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("YAML frontmatter error at {path}: {source}")]
    Yaml {
        path: Utf8PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("agent `{0}` is not known")]
    UnknownAgent(String),
    #[error("skill `{0}` is not acquired")]
    SkillNotAcquired(String),
    #[error("skill `{skill}` is still enabled for {agents}; disable it first or pass --all")]
    SkillStillEnabled { skill: String, agents: String },
    #[error("project skill link would point outside the repository: {0}")]
    ProjectLinkEscape(Utf8PathBuf),
    #[error("cannot create symlinks in {path}: {detail}")]
    SymlinkUnsupported { path: Utf8PathBuf, detail: String },
    #[error("agent skill path already exists and is not managed by yasm: {0}")]
    AgentPathConflict(Utf8PathBuf),
    #[error("agent skill symlink points somewhere else: {link} -> {target}")]
    AgentSymlinkConflict {
        link: Utf8PathBuf,
        target: Utf8PathBuf,
    },
}

pub fn io(path: impl Into<Utf8PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
