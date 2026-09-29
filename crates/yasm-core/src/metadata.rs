use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde_yaml::{Mapping, Value};

use crate::types::SkillName;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Codex,
    Claude,
    Pi,
}

impl Harness {
    pub const ALL: &'static [Self] = &[Self::Codex, Self::Claude, Self::Pi];

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Pi => "Pi",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationStatus {
    Automatic,
    ManualOnly,
    Unknown,
}

impl InvocationStatus {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::ManualOnly => "manual only",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvocationMetadata {
    pub codex: InvocationStatus,
    pub claude: InvocationStatus,
    pub pi: InvocationStatus,
}

impl InvocationMetadata {
    pub const fn status(&self, harness: Harness) -> InvocationStatus {
        match harness {
            Harness::Codex => self.codex,
            Harness::Claude => self.claude,
            Harness::Pi => self.pi,
        }
    }
}

impl Default for InvocationMetadata {
    fn default() -> Self {
        Self {
            codex: InvocationStatus::Automatic,
            claude: InvocationStatus::Automatic,
            pi: InvocationStatus::Automatic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetadataDiagnostic {
    pub path: Utf8PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillMetadata {
    pub name: Option<SkillName>,
    pub description: Option<String>,
    pub invocation: InvocationMetadata,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<MetadataDiagnostic>,
}

pub fn read_skill_metadata(skill_dir: &Utf8Path) -> SkillMetadata {
    let mut metadata = SkillMetadata {
        name: None,
        description: None,
        invocation: InvocationMetadata::default(),
        diagnostics: Vec::new(),
    };

    read_skill_frontmatter(skill_dir, &mut metadata);
    read_openai_metadata(skill_dir, &mut metadata);
    metadata
}

fn read_skill_frontmatter(skill_dir: &Utf8Path, metadata: &mut SkillMetadata) {
    let path = skill_dir.join("SKILL.md");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            metadata.invocation.claude = InvocationStatus::Unknown;
            metadata.invocation.pi = InvocationStatus::Unknown;
            diagnose(
                metadata,
                path,
                format!("could not read skill metadata: {error}"),
            );
            return;
        }
    };
    let yaml = match frontmatter(&content) {
        Ok(yaml) => yaml,
        Err(message) => {
            metadata.invocation.claude = InvocationStatus::Unknown;
            metadata.invocation.pi = InvocationStatus::Unknown;
            diagnose(metadata, path, message);
            return;
        }
    };
    let mapping = match yaml_mapping(&yaml) {
        Ok(mapping) => mapping,
        Err(message) => {
            metadata.invocation.claude = InvocationStatus::Unknown;
            metadata.invocation.pi = InvocationStatus::Unknown;
            diagnose(metadata, path, message);
            return;
        }
    };

    metadata.name = string_field(&mapping, "name", true, &path, &mut metadata.diagnostics)
        .and_then(|name| match SkillName::parse(name.clone()) {
            Ok(name) => Some(name),
            Err(error) => {
                metadata.diagnostics.push(MetadataDiagnostic {
                    path: path.clone(),
                    message: format!("invalid `name`: {error}"),
                });
                None
            }
        });
    metadata.description = string_field(
        &mapping,
        "description",
        false,
        &path,
        &mut metadata.diagnostics,
    );

    metadata.invocation.claude = claude_boolean_policy(
        &mapping,
        "disable-model-invocation",
        InvocationStatus::ManualOnly,
        InvocationStatus::Automatic,
        &path,
        &mut metadata.diagnostics,
    );
    metadata.invocation.pi = boolean_policy(
        &mapping,
        "disable-model-invocation",
        InvocationStatus::ManualOnly,
        InvocationStatus::Automatic,
        "Pi",
        &path,
        &mut metadata.diagnostics,
    );
}

fn read_openai_metadata(skill_dir: &Utf8Path, metadata: &mut SkillMetadata) {
    let path = skill_dir.join("agents/openai.yaml");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            metadata.invocation.codex = InvocationStatus::Unknown;
            diagnose(
                metadata,
                path,
                format!("could not read Codex metadata: {error}"),
            );
            return;
        }
    };
    let root = match yaml_mapping_allow_empty(&content) {
        Ok(mapping) => mapping,
        Err(message) => {
            metadata.invocation.codex = InvocationStatus::Unknown;
            diagnose(metadata, path, message);
            return;
        }
    };
    let Some(policy) = mapping_field(&root, "policy") else {
        return;
    };
    let Value::Mapping(policy) = policy else {
        metadata.invocation.codex = InvocationStatus::Unknown;
        diagnose(
            metadata,
            path,
            "invalid `policy`: expected a mapping".to_string(),
        );
        return;
    };

    metadata.invocation.codex = boolean_policy(
        policy,
        "allow_implicit_invocation",
        InvocationStatus::Automatic,
        InvocationStatus::ManualOnly,
        "Codex",
        &path,
        &mut metadata.diagnostics,
    );
}

fn frontmatter(content: &str) -> Result<String, String> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return Err("SKILL.md must start with YAML frontmatter".to_string());
    }

    let mut yaml = String::new();
    for line in lines {
        if line == "---" {
            return Ok(yaml);
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    Err("SKILL.md has unterminated YAML frontmatter".to_string())
}

fn yaml_mapping(yaml: &str) -> Result<Mapping, String> {
    let value = serde_yaml::from_str::<Value>(yaml)
        .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
    match value {
        Value::Mapping(mapping) => Ok(mapping),
        _ => Err("invalid YAML frontmatter: expected a mapping".to_string()),
    }
}

fn yaml_mapping_allow_empty(yaml: &str) -> Result<Mapping, String> {
    let value = serde_yaml::from_str::<Value>(yaml)
        .map_err(|error| format!("invalid Codex metadata YAML: {error}"))?;
    match value {
        Value::Null => Ok(Mapping::new()),
        Value::Mapping(mapping) => Ok(mapping),
        _ => Err("invalid Codex metadata YAML: expected a mapping".to_string()),
    }
}

fn mapping_field<'a>(mapping: &'a Mapping, name: &str) -> Option<&'a Value> {
    mapping.get(Value::String(name.to_string()))
}

fn string_field(
    mapping: &Mapping,
    name: &str,
    required: bool,
    path: &Utf8Path,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> Option<String> {
    match mapping_field(mapping, name) {
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => {
            diagnostics.push(MetadataDiagnostic {
                path: path.to_path_buf(),
                message: format!("invalid `{name}`: expected a string"),
            });
            None
        }
        None if required => {
            diagnostics.push(MetadataDiagnostic {
                path: path.to_path_buf(),
                message: format!("missing required `{name}` field"),
            });
            None
        }
        None => None,
    }
}

fn boolean_policy(
    mapping: &Mapping,
    name: &str,
    when_true: InvocationStatus,
    when_false: InvocationStatus,
    harness: &str,
    path: &Utf8Path,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> InvocationStatus {
    match mapping_field(mapping, name) {
        Some(Value::Bool(true)) => when_true,
        Some(Value::Bool(false)) | None => when_false,
        Some(_) => {
            diagnostics.push(MetadataDiagnostic {
                path: path.to_path_buf(),
                message: format!("invalid `{name}` for {harness}: expected a boolean"),
            });
            InvocationStatus::Unknown
        }
    }
}

fn claude_boolean_policy(
    mapping: &Mapping,
    name: &str,
    when_true: InvocationStatus,
    when_false: InvocationStatus,
    path: &Utf8Path,
    diagnostics: &mut Vec<MetadataDiagnostic>,
) -> InvocationStatus {
    let Some(value) = mapping_field(mapping, name) else {
        return when_false;
    };
    let parsed = match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) if value.as_i64() == Some(1) => Some(true),
        Value::Number(value) if value.as_i64() == Some(0) => Some(false),
        Value::String(value)
            if ["true", "yes", "on", "1"]
                .iter()
                .any(|candidate| value.eq_ignore_ascii_case(candidate)) =>
        {
            Some(true)
        }
        Value::String(value)
            if ["false", "no", "off", "0"]
                .iter()
                .any(|candidate| value.eq_ignore_ascii_case(candidate)) =>
        {
            Some(false)
        }
        _ => None,
    };

    match parsed {
        Some(true) => when_true,
        Some(false) => when_false,
        None => {
            diagnostics.push(MetadataDiagnostic {
                path: path.to_path_buf(),
                message: format!("invalid `{name}` for Claude: expected a boolean"),
            });
            InvocationStatus::Unknown
        }
    }
}

fn diagnose(metadata: &mut SkillMetadata, path: Utf8PathBuf, message: String) {
    metadata
        .diagnostics
        .push(MetadataDiagnostic { path, message });
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn skill_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let temp = tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(temp.path().join("demo")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        (temp, dir)
    }

    fn write_skill(dir: &Utf8Path, fields: &str) {
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: demo\ndescription: Demo skill\n{fields}---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn missing_invocation_settings_default_to_automatic() {
        let (_temp, dir) = skill_dir();
        write_skill(&dir, "");

        let metadata = read_skill_metadata(&dir);

        assert_eq!(metadata.name.unwrap().as_str(), "demo");
        assert_eq!(metadata.description.as_deref(), Some("Demo skill"));
        assert_eq!(metadata.invocation, InvocationMetadata::default());
        assert!(metadata.diagnostics.is_empty());
    }

    #[test]
    fn shared_and_codex_settings_are_independent() {
        let (_temp, dir) = skill_dir();
        write_skill(&dir, "disable-model-invocation: true\n");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::write(
            dir.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: true\nunknown:\n  - preserved\n",
        )
        .unwrap();

        let metadata = read_skill_metadata(&dir);

        assert_eq!(metadata.invocation.codex, InvocationStatus::Automatic);
        assert_eq!(metadata.invocation.claude, InvocationStatus::ManualOnly);
        assert_eq!(metadata.invocation.pi, InvocationStatus::ManualOnly);
        assert!(metadata.diagnostics.is_empty());
    }

    #[test]
    fn combined_manual_only_settings_cover_every_harness() {
        let (_temp, dir) = skill_dir();
        write_skill(&dir, "disable-model-invocation: true\n");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::write(
            dir.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: false\n",
        )
        .unwrap();

        let metadata = read_skill_metadata(&dir);

        for harness in Harness::ALL {
            assert_eq!(
                metadata.invocation.status(*harness),
                InvocationStatus::ManualOnly
            );
        }
    }

    #[test]
    fn claude_accepts_extended_boolean_spellings_that_are_invalid_for_pi() {
        for (value, expected) in [
            ("yes", InvocationStatus::ManualOnly),
            ("YeS", InvocationStatus::ManualOnly),
            ("on", InvocationStatus::ManualOnly),
            ("ON", InvocationStatus::ManualOnly),
            ("1", InvocationStatus::ManualOnly),
            ("no", InvocationStatus::Automatic),
            ("No", InvocationStatus::Automatic),
            ("off", InvocationStatus::Automatic),
            ("OFF", InvocationStatus::Automatic),
            ("0", InvocationStatus::Automatic),
        ] {
            let (_temp, dir) = skill_dir();
            write_skill(&dir, &format!("disable-model-invocation: {value}\n"));

            let metadata = read_skill_metadata(&dir);

            assert_eq!(metadata.invocation.claude, expected, "value: {value}");
            assert_eq!(
                metadata.invocation.pi,
                InvocationStatus::Unknown,
                "value: {value}"
            );
            assert_eq!(metadata.diagnostics.len(), 1, "value: {value}");
            assert!(metadata.diagnostics[0].message.contains("for Pi"));
        }
    }

    #[test]
    fn invalid_fields_only_make_their_harness_status_unknown() {
        let (_temp, dir) = skill_dir();
        write_skill(&dir, "disable-model-invocation: manual\n");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::write(
            dir.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: disabled\n",
        )
        .unwrap();

        let metadata = read_skill_metadata(&dir);

        assert_eq!(metadata.invocation.codex, InvocationStatus::Unknown);
        assert_eq!(metadata.invocation.claude, InvocationStatus::Unknown);
        assert_eq!(metadata.invocation.pi, InvocationStatus::Unknown);
        assert_eq!(metadata.diagnostics.len(), 3);
    }

    #[test]
    fn malformed_skill_frontmatter_does_not_hide_codex_default() {
        let (_temp, dir) = skill_dir();
        std::fs::write(dir.join("SKILL.md"), "---\nname: [\n---\n").unwrap();

        let metadata = read_skill_metadata(&dir);

        assert_eq!(metadata.invocation.codex, InvocationStatus::Automatic);
        assert_eq!(metadata.invocation.claude, InvocationStatus::Unknown);
        assert_eq!(metadata.invocation.pi, InvocationStatus::Unknown);
        assert_eq!(metadata.diagnostics.len(), 1);
    }

    #[test]
    fn unreadable_metadata_files_report_unknown_without_failing() {
        let (_temp, dir) = skill_dir();
        std::fs::create_dir(dir.join("SKILL.md")).unwrap();
        std::fs::create_dir_all(dir.join("agents/openai.yaml")).unwrap();

        let metadata = read_skill_metadata(&dir);

        assert!(metadata.name.is_none());
        assert_eq!(metadata.invocation.codex, InvocationStatus::Unknown);
        assert_eq!(metadata.invocation.claude, InvocationStatus::Unknown);
        assert_eq!(metadata.invocation.pi, InvocationStatus::Unknown);
        assert_eq!(metadata.diagnostics.len(), 2);
    }
}
