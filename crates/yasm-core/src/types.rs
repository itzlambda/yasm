use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SkillName(String);

impl SkillName {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.trim().is_empty() || value.contains('/') || value.contains('\\') {
            return Err(Error::InvalidValue {
                kind: "skill name",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillName {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::str::FromStr for SkillName {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::fmt::Display for SkillName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SkillId(String);

impl SkillId {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.contains('/')
            || value.contains('\\')
        {
            return Err(Error::InvalidValue {
                kind: "skill id",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::str::FromStr for SkillId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SkillPath(String);

impl SkillPath {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.starts_with('/')
            || value.split('/').any(|part| part == "." || part == "..")
            || value.contains('\\')
        {
            return Err(Error::InvalidValue {
                kind: "skill path",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillPath {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::str::FromStr for SkillPath {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Bundled,
    Github,
    Git,
    Local,
    Owned,
}

impl SourceKind {
    pub fn is_git(&self) -> bool {
        matches!(self, Self::Github | Self::Git)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpec {
    pub kind: SourceKind,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<GitRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<SkillPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<GitRef>,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct GitRef(String);

impl GitRef {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.trim().is_empty()
            || value.contains("..")
            || value.starts_with('-')
            || value.contains('\\')
        {
            return Err(Error::InvalidValue {
                kind: "git ref",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitRef {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::str::FromStr for GitRef {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

pub fn sanitize_skill_id(name: &SkillName) -> Result<SkillId> {
    let mut out = String::new();
    let mut previous_dash = false;
    for ch in name.as_str().chars() {
        let next = if ch.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if ch == '-' || ch == '_' || ch.is_whitespace() {
            if previous_dash {
                None
            } else {
                previous_dash = true;
                Some('-')
            }
        } else {
            None
        };
        if let Some(ch) = next {
            out.push(ch);
        }
    }
    let out = out.trim_matches('-').to_string();
    SkillId::parse(out)
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::str::FromStr;

    use serde::de::DeserializeOwned;
    use serde::Serialize;

    use crate::error::Error;
    use crate::types::{GitRef, SkillId, SkillName, SkillPath};

    fn assert_validated_string<T>(valid: &str, invalid: &[&str])
    where
        T: FromStr<Err = Error>
            + TryFrom<String, Error = Error>
            + Serialize
            + DeserializeOwned
            + Debug
            + PartialEq,
    {
        let parsed = T::from_str(valid).unwrap();
        assert_eq!(T::try_from(valid.to_owned()).unwrap(), parsed);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), valid);
        assert_eq!(serde_json::from_value::<T>(valid.into()).unwrap(), parsed);

        for value in invalid {
            assert!(T::from_str(value).is_err(), "FromStr accepted {value:?}");
            assert!(
                T::try_from((*value).to_owned()).is_err(),
                "TryFrom accepted {value:?}"
            );
            assert!(
                serde_json::from_value::<T>((*value).into()).is_err(),
                "Deserialize accepted {value:?}"
            );
        }
    }

    #[test]
    fn validated_string_types_reject_invalid_deserialization() {
        assert_validated_string::<SkillName>("frontend-design", &["", "  ", "a/b", "a\\b"]);
        assert_validated_string::<SkillId>("frontend-design", &["", ".", "..", "a/b", "a\\b"]);
        assert_validated_string::<SkillPath>(
            "skills/demo/SKILL.md",
            &["", "/abs", "a/./b", "a/../b", "a\\b"],
        );
        assert_validated_string::<GitRef>("feature/demo", &["", "  ", "a..b", "-branch", "a\\b"]);
    }
}
