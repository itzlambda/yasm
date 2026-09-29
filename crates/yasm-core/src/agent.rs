use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::types::SkillId;

// Keep the enum, registry inventory, and accepted CLI names in one declaration.
macro_rules! link_targets {
    ($($variant:ident => ($id:literal, $display_name:literal, $skill_dir:literal)),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum LinkTarget {
            $($variant),+
        }

        impl LinkTarget {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $id),+ }
            }

            pub const fn display_name(self) -> &'static str {
                match self { $(Self::$variant => $display_name),+ }
            }

            pub const fn skill_dir(self) -> &'static str {
                match self { $(Self::$variant => $skill_dir),+ }
            }
        }

        impl std::str::FromStr for LinkTarget {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self> {
                match value {
                    $($id => Ok(Self::$variant),)+
                    _ => Err(Error::UnknownAgent(value.to_owned())),
                }
            }
        }
    };
}

link_targets! {
    Universal => ("universal", "Universal", ".agents/skills"),
    Claude => ("claude", "Claude", ".claude/skills"),
}

pub type BuiltInAgent = LinkTarget;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct AgentId(String);

impl From<LinkTarget> for AgentId {
    fn from(agent: LinkTarget) -> Self {
        Self(agent.as_str().to_owned())
    }
}

impl AgentId {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.contains('/') || value.contains('\\') {
            return Err(Error::InvalidValue {
                kind: "agent id",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::str::FromStr for AgentId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: AgentId,
    pub display_name: &'static str,
    pub skill_dir: Utf8PathBuf,
}

impl Agent {
    pub fn skill_link(&self, skill_id: &SkillId) -> Utf8PathBuf {
        self.skill_dir.join(skill_id.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct AgentRegistry {
    agents: Vec<Agent>,
}

impl AgentRegistry {
    pub fn discover() -> Result<Self> {
        if let Ok(root) = std::env::var("YASM_AGENT_SKILLS_ROOT") {
            if root.is_empty() {
                return Err(Error::Message(
                    "YASM_AGENT_SKILLS_ROOT is set but empty".to_string(),
                ));
            }
            return Ok(Self::with_home(Utf8PathBuf::from(root)));
        }

        let home = etcetera::home_dir().map_err(|source| {
            Error::Message(format!("could not discover home directory: {source}"))
        })?;
        let home = Utf8PathBuf::from_path_buf(home)
            .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
        Ok(Self::with_home(home))
    }

    pub fn with_home(root: Utf8PathBuf) -> Self {
        let agents = LinkTarget::ALL
            .iter()
            .map(|&agent| Agent {
                id: agent.into(),
                display_name: agent.display_name(),
                skill_dir: root.join(agent.skill_dir()),
            })
            .collect();
        Self { agents }
    }

    pub fn get(&self, id: &str) -> Result<&Agent> {
        self.agents
            .iter()
            .find(|agent| agent.id.as_str() == id)
            .ok_or_else(|| Error::UnknownAgent(id.to_string()))
    }

    pub fn all(&self) -> &[Agent] {
        &self.agents
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use crate::agent::{AgentId, AgentRegistry};

    #[test]
    fn link_targets_use_expected_skill_directories() {
        let registry = AgentRegistry::with_home(Utf8PathBuf::from("/tmp/yasm-agents"));

        assert_eq!(
            registry.get("universal").unwrap().skill_dir.as_str(),
            "/tmp/yasm-agents/.agents/skills"
        );
        assert_eq!(registry.get("universal").unwrap().display_name, "Universal");
        assert_eq!(
            registry.get("claude").unwrap().skill_dir.as_str(),
            "/tmp/yasm-agents/.claude/skills"
        );
        assert_eq!(registry.get("claude").unwrap().display_name, "Claude");
    }

    #[test]
    fn agent_id_rejects_invalid_deserialization() {
        let valid = AgentId::parse("universal").unwrap();
        assert_eq!("universal".parse::<AgentId>().unwrap(), valid);
        assert_eq!(AgentId::try_from("universal".to_owned()).unwrap(), valid);
        assert_eq!(serde_json::to_value(&valid).unwrap(), "universal");
        assert_eq!(
            serde_json::from_str::<AgentId>("\"universal\"").unwrap(),
            valid
        );

        for invalid in ["", "a/b", "a\\b"] {
            assert!(invalid.parse::<AgentId>().is_err());
            assert!(AgentId::try_from(invalid.to_owned()).is_err());
            assert!(serde_json::from_value::<AgentId>(invalid.into()).is_err());
        }
    }
}
