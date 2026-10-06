use std::str::FromStr;

use camino::Utf8PathBuf;
use yasm_core::{Error, GitRef, Result, SourceKind, SourceSpec};

use crate::git::{valid_repo_part, GitRemote};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceInput {
    Bundled,
    Git(GitRemote),
    Local(Utf8PathBuf),
    GitHubShorthand {
        owner: String,
        repository: String,
        git_ref: Option<GitRef>,
    },
    GitHubUrl {
        owner: String,
        repository: String,
        subpath: Option<yasm_core::SkillPath>,
    },
}

impl SourceInput {
    pub fn parse_github(input: &str) -> Result<Self> {
        if input.trim().is_empty() {
            return Err(Error::InvalidValue {
                kind: "GitHub source",
                value: input.to_string(),
            });
        }
        if input.starts_with("http://") || input.starts_with("https://") {
            parse_github_url(input)
        } else {
            parse_github_shorthand(input)
        }
    }

    pub fn parse_remote(input: &str) -> Result<Self> {
        if !input.contains("://") && input.contains(':') && input.contains('@') {
            return Ok(Self::Git(input.parse()?));
        }
        if input.starts_with("ssh://") {
            return Err(Error::Message(
                "ssh:// sources are not supported; use SCP-style user@host:path".to_string(),
            ));
        }
        if input.contains("://") && !input.starts_with("https://") && !input.starts_with("http://")
        {
            return Err(Error::Message("unsupported URL source; use a GitHub HTTPS repository URL or SCP-style user@host:path".to_string()));
        }
        Self::parse_github(input)
    }

    pub fn into_spec(self) -> Result<SourceSpec> {
        match self {
            Self::Git(remote) => Ok(remote.into()),
            Self::Bundled => Ok(SourceSpec {
                kind: SourceKind::Bundled,
                path: "self".to_string(),
                r#ref: None,
                subpath: None,
            }),
            Self::Local(path) => local_source_spec(path),
            Self::GitHubShorthand {
                owner,
                repository,
                git_ref,
            } => Ok(SourceSpec {
                kind: SourceKind::Github,
                path: github_clone_url(&owner, &repository),
                r#ref: git_ref,
                subpath: None,
            }),
            Self::GitHubUrl {
                owner,
                repository,
                subpath,
            } => Ok(SourceSpec {
                kind: SourceKind::Github,
                path: github_clone_url(&owner, &repository),
                r#ref: subpath
                    .as_ref()
                    .map(|_| GitRef::parse("main"))
                    .transpose()?,
                subpath,
            }),
        }
    }
}

impl FromStr for SourceInput {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        if input.trim().is_empty() {
            return Err(Error::InvalidValue {
                kind: "source",
                value: input.to_string(),
            });
        }

        if input == "self" {
            return Ok(Self::Bundled);
        }

        if looks_like_local_path(input) {
            return Ok(Self::Local(Utf8PathBuf::from(input)));
        }

        Self::parse_remote(input)
    }
}

fn local_source_spec(path: Utf8PathBuf) -> Result<SourceSpec> {
    let expanded = expand_home_relative(&path)?;
    let canonical = std::fs::canonicalize(&expanded).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::Message(format!("local source path does not exist: {path}"))
        } else {
            Error::Message(format!(
                "failed to resolve local source path {path}: {source}"
            ))
        }
    })?;
    let canonical = Utf8PathBuf::from_path_buf(canonical)
        .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;

    Ok(SourceSpec {
        kind: SourceKind::Local,
        path: canonical.to_string(),
        r#ref: None,
        subpath: None,
    })
}

fn expand_home_relative(path: &camino::Utf8Path) -> Result<Utf8PathBuf> {
    let suffix = match path.as_str() {
        "~" => Some(""),
        value if value.starts_with("~/") => value.strip_prefix("~/"),
        value if value.starts_with('~') => {
            return Err(Error::Message(format!(
                "unsupported home-relative local source path `{value}`; use `~` or `~/path`"
            )))
        }
        _ => None,
    };
    let Some(suffix) = suffix else {
        return Ok(path.to_path_buf());
    };

    let home = etcetera::home_dir()
        .map_err(|source| Error::Message(format!("could not discover home directory: {source}")))?;
    let home = Utf8PathBuf::from_path_buf(home)
        .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
    Ok(home.join(suffix))
}

fn looks_like_local_path(input: &str) -> bool {
    input.starts_with('.')
        || input.starts_with('/')
        || input.starts_with('~')
        || std::path::Path::new(input).exists()
}

fn parse_github_shorthand(input: &str) -> Result<SourceInput> {
    let (repo, git_ref) = split_ref(input)?;
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| !valid_repo_part(part)) {
        return Err(Error::InvalidValue {
            kind: "GitHub shorthand",
            value: input.to_string(),
        });
    }

    let repository = parts[1].trim_end_matches(".git");
    if !valid_repo_part(repository) {
        return Err(Error::InvalidValue {
            kind: "GitHub shorthand",
            value: input.to_string(),
        });
    }

    Ok(SourceInput::GitHubShorthand {
        owner: parts[0].to_string(),
        repository: repository.to_string(),
        git_ref,
    })
}

fn parse_github_url(input: &str) -> Result<SourceInput> {
    let Some(rest) = input
        .strip_prefix("https://github.com/")
        .or_else(|| input.strip_prefix("http://github.com/"))
    else {
        return Err(Error::Message(
            "unsupported URL source; only GitHub repository URLs and SCP-style SSH addresses are supported".to_string()
        ));
    };

    if rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return Err(Error::Message(
            "unsupported GitHub repository URL; credentials, queries and fragments are not allowed"
                .to_string(),
        ));
    }
    let rest = rest.trim_end_matches('/');
    let parts: Vec<_> = rest.split('/').collect();
    if parts.len() > 2 {
        if parts.len() < 5 || parts[2] != "tree" {
            return Err(Error::Message(format!(
                "unsupported GitHub URL path: {input}; use https://github.com/<owner>/<repo> or https://github.com/<owner>/<repo>/tree/main/<directory>"
            )));
        }
        if parts[3] != "main" {
            return Err(Error::Message(format!(
                "unsupported GitHub tree ref `{}` in {input}; only `main` tree URLs are supported",
                parts[3]
            )));
        }
        if parts[4..].iter().any(|part| part.is_empty()) {
            return Err(Error::InvalidValue {
                kind: "GitHub tree URL directory",
                value: input.to_string(),
            });
        }
    }
    if parts.len() != 2 && parts.len() < 5 {
        return Err(Error::Message(format!(
            "unsupported GitHub URL path: {input}; use https://github.com/<owner>/<repo> or https://github.com/<owner>/<repo>/tree/main/<directory>"
        )));
    }
    if !valid_repo_part(parts[0]) {
        return Err(Error::InvalidValue {
            kind: "GitHub repository URL",
            value: input.to_string(),
        });
    }

    let repository = parts[1].trim_end_matches(".git");
    if !valid_repo_part(repository) {
        return Err(Error::InvalidValue {
            kind: "GitHub repository URL",
            value: input.to_string(),
        });
    }

    let _: GitRemote = github_clone_url(parts[0], repository).parse()?;

    let subpath = if parts.len() == 2 {
        None
    } else {
        Some(yasm_core::SkillPath::parse(parts[4..].join("/"))?)
    };
    Ok(SourceInput::GitHubUrl {
        owner: parts[0].to_string(),
        repository: repository.to_string(),
        subpath,
    })
}

fn split_ref(input: &str) -> Result<(&str, Option<GitRef>)> {
    if let Some((repo, git_ref)) = input.rsplit_once('@') {
        Ok((repo, Some(GitRef::parse(git_ref)?)))
    } else {
        Ok((input, None))
    }
}

fn github_clone_url(owner: &str, repository: &str) -> String {
    format!(
        "https://github.com/{}/{}.git",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use crate::source::*;

    #[test]
    fn parses_github_shorthand_with_ref() {
        let input: SourceInput = "anthropics/claude-code@main".parse().unwrap();
        assert!(matches!(
            input,
            SourceInput::GitHubShorthand {
                ref owner,
                ref repository,
                git_ref: Some(ref git_ref),
            } if owner == "anthropics"
                && repository == "claude-code"
                && git_ref.as_str() == "main"
        ));
    }

    #[test]
    fn repo_sources_without_ref_use_the_remote_default_branch() {
        assert_eq!(
            "owner/repo"
                .parse::<SourceInput>()
                .unwrap()
                .into_spec()
                .unwrap()
                .r#ref,
            None
        );
        assert_eq!(
            "https://github.com/owner/repo"
                .parse::<SourceInput>()
                .unwrap()
                .into_spec()
                .unwrap()
                .r#ref,
            None
        );
    }

    #[test]
    fn parses_repository_root_url() {
        let input: SourceInput = "https://github.com/anthropics/claude-code/"
            .parse()
            .unwrap();
        assert!(matches!(
            input,
            SourceInput::GitHubUrl {
                ref owner,
                ref repository,
                subpath: None,
            } if owner == "anthropics" && repository == "claude-code"
        ));
    }

    #[test]
    fn parses_main_branch_tree_url_with_subpath() {
        let source = "https://github.com/anthropics/claude-code/tree/main/plugins/frontend-design"
            .parse::<SourceInput>()
            .unwrap()
            .into_spec()
            .unwrap();

        assert_eq!(source.path, "https://github.com/anthropics/claude-code.git");
        assert_eq!(source.r#ref.unwrap().as_str(), "main");
        assert_eq!(source.subpath.unwrap().as_str(), "plugins/frontend-design");
    }

    #[test]
    fn rejects_non_main_tree_refs_actionably() {
        let error: yasm_core::Error =
            "https://github.com/anthropics/claude-code/tree/develop/plugins/frontend-design"
                .parse::<SourceInput>()
                .unwrap_err();

        assert!(error.to_string().contains("only `main` tree URLs"));
    }

    #[test]
    fn rejects_tree_urls_without_a_safe_directory() {
        for input in [
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo/tree/main/../skills",
            "https://github.com/owner/repo/blob/main/SKILL.md",
        ] {
            assert!(
                input.parse::<SourceInput>().is_err(),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_unsupported_remote_url_and_ssh_scheme_actionably() {
        let error: yasm_core::Error = "https://gitlab.com/owner/repo"
            .parse::<SourceInput>()
            .unwrap_err();
        assert!(error.to_string().contains("only GitHub repository URLs"));

        let error = "ssh://git@github.com/owner/repo.git"
            .parse::<SourceInput>()
            .unwrap_err();
        assert!(error.to_string().contains("use SCP-style"));
    }

    #[test]
    fn parses_local_source_as_its_own_variant() {
        assert!(matches!(
            "./skills".parse::<SourceInput>().unwrap(),
            SourceInput::Local(path) if path == "./skills"
        ));
    }

    #[test]
    fn reserves_self_but_keeps_explicit_local_self_paths() {
        assert_eq!("self".parse::<SourceInput>().unwrap(), SourceInput::Bundled);
        assert!(matches!(
            "./self".parse::<SourceInput>().unwrap(),
            SourceInput::Local(path) if path == "./self"
        ));
    }

    #[test]
    fn rejects_traversal_shorthand() {
        assert!(matches!(
            "../repo".parse::<SourceInput>().unwrap(),
            SourceInput::Local(path) if path == "../repo"
        ));
        assert!("owner/../repo".parse::<SourceInput>().is_err());
    }
}

#[cfg(test)]
mod ssh_source_tests {
    use crate::SourceInput;
    use yasm_core::SourceKind;

    #[test]
    fn scp_username_is_not_a_git_ref() {
        let address = "git@work-alias:team/private-skills.git";
        let spec = address.parse::<SourceInput>().unwrap().into_spec().unwrap();
        assert_eq!(spec.kind, SourceKind::Git);
        assert_eq!(spec.path, address);
        assert!(spec.r#ref.is_none());
    }

    #[test]
    fn rejects_credential_urls_without_echoing_the_secret() {
        for address in [
            "https://user:top-secret@github.com/team/repo",
            "https://github.com/team/repo?token=top-secret",
            "git:top-secret@host:repo",
        ] {
            let error = address.parse::<SourceInput>().unwrap_err();
            assert!(!format!("{error:?} {error}").contains("top-secret"));
        }
    }
}
