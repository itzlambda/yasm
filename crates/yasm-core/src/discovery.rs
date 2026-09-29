use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::error::{io, Error, Result};
use crate::types::{SkillName, SkillPath};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    pub name: SkillName,
    pub description: Option<String>,
    pub skill_path: SkillPath,
    pub directory: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSkill {
    pub path: Utf8PathBuf,
    pub reason: String,
}

#[derive(Debug)]
pub struct SkillDiscovery {
    pub skills: Vec<DiscoveredSkill>,
    pub skipped: Vec<SkippedSkill>,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: Option<String>,
}

pub fn discover_skills(root: &Utf8Path) -> Result<Vec<DiscoveredSkill>> {
    Ok(discover_skills_with_diagnostics(root)?.skills)
}

pub fn discover_skills_with_diagnostics(root: &Utf8Path) -> Result<SkillDiscovery> {
    let mut discovery = SkillDiscovery {
        skills: Vec::new(),
        skipped: Vec::new(),
    };
    visit(root, root, &mut discovery)?;
    discovery
        .skills
        .sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    discovery
        .skipped
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(discovery)
}

fn visit(root: &Utf8Path, dir: &Utf8Path, discovery: &mut SkillDiscovery) -> Result<()> {
    let skill_file = dir.join("SKILL.md");
    if skill_file.exists() {
        match parse_skill_file(root, &skill_file) {
            Ok(skill) => discovery.skills.push(skill),
            Err(error) => {
                let path = skill_file
                    .strip_prefix(root)
                    .unwrap_or(&skill_file)
                    .to_path_buf();
                let reason = match &error {
                    Error::Yaml { source, .. } => {
                        format!("invalid YAML frontmatter: {source}")
                    }
                    Error::Io { source, .. } => format!("could not read skill: {source}"),
                    _ => error.to_string(),
                };
                discovery.skipped.push(SkippedSkill { path, reason });
            }
        }
        // A directory with SKILL.md is a skill root; do not treat nested
        // directories as additional skills.
        return Ok(());
    }

    for entry in std::fs::read_dir(dir).map_err(|source| io(dir, source))? {
        let entry = entry.map_err(|source| io(dir, source))?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
        if entry
            .file_type()
            .map_err(|source| io(&path, source))?
            .is_dir()
        {
            if matches!(path.file_name(), Some(".git" | "node_modules")) {
                continue;
            }
            visit(root, &path, discovery)?;
        }
    }
    Ok(())
}

pub fn parse_skill_file(root: &Utf8Path, skill_file: &Utf8Path) -> Result<DiscoveredSkill> {
    let content = std::fs::read_to_string(skill_file).map_err(|source| io(skill_file, source))?;
    let frontmatter = parse_frontmatter(skill_file, &content)?;
    let relative = skill_file
        .strip_prefix(root)
        .map_err(|_| Error::Message(format!("{skill_file} is not under {root}")))?;
    let directory = skill_file
        .parent()
        .ok_or_else(|| Error::Message(format!("{skill_file} has no parent directory")))?
        .to_path_buf();

    Ok(DiscoveredSkill {
        name: SkillName::parse(frontmatter.name)?,
        description: frontmatter.description,
        skill_path: SkillPath::parse(relative.as_str().replace('\\', "/"))?,
        directory,
    })
}

fn parse_frontmatter(path: &Utf8Path, content: &str) -> Result<SkillFrontmatter> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return Err(Error::Message(format!(
            "{path} must start with YAML frontmatter"
        )));
    }

    let mut yaml = String::new();
    for line in lines {
        if line == "---" {
            return serde_yaml::from_str(&yaml).map_err(|source| Error::Yaml {
                path: path.to_path_buf(),
                source,
            });
        }
        yaml.push_str(line);
        yaml.push('\n');
    }

    Err(Error::Message(format!(
        "{path} has unterminated YAML frontmatter"
    )))
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_nested_skill_frontmatter() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let skill_dir = root.join("skills/frontend-design");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: frontend-design\ndescription: Design UI\n---\nbody\n",
        )
        .unwrap();

        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name.as_str(), "frontend-design");
        assert_eq!(skills[0].description.as_deref(), Some("Design UI"));
        assert_eq!(
            skills[0].skill_path.as_str(),
            "skills/frontend-design/SKILL.md"
        );
    }

    #[test]
    fn skips_invalid_skill_and_continues_discovery() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let broken_dir = root.join("agent-compatibility/skills/check-agent-compatibility");
        std::fs::create_dir_all(&broken_dir).unwrap();
        std::fs::write(
            broken_dir.join("SKILL.md"),
            "---\nname: check-agent-compatibility\ndescription: Run the full repository compatibility pass: scanner score, startup path, validation loop, and docs reliability.\n---\nbody\n",
        )
        .unwrap();
        let valid_dir = root.join("skills/frontend-design");
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::write(
            valid_dir.join("SKILL.md"),
            "---\nname: frontend-design\ndescription: Design UI\n---\nbody\n",
        )
        .unwrap();

        let discovery = discover_skills_with_diagnostics(&root).unwrap();
        assert_eq!(discovery.skills.len(), 1);
        assert_eq!(discovery.skills[0].name.as_str(), "frontend-design");
        assert_eq!(discovery.skipped.len(), 1);
        assert_eq!(
            discovery.skipped[0].path,
            Utf8PathBuf::from("agent-compatibility/skills/check-agent-compatibility/SKILL.md")
        );
        assert!(discovery.skipped[0]
            .reason
            .contains("invalid YAML frontmatter"));
        assert!(discovery.skipped[0]
            .reason
            .contains("mapping values are not allowed"));
    }

    #[test]
    fn convenience_api_returns_only_valid_skills() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: broken\ndescription: \"Broken: description\n---\nbody\n",
        )
        .unwrap();

        assert!(discover_skills(&root).unwrap().is_empty());
    }

    fn write_skill(dir: &Utf8Path, name: &str, description: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn skips_git_and_node_modules_while_walking() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        write_skill(
            &root.join("skills/frontend-design"),
            "frontend-design",
            "Design UI",
        );
        write_skill(
            &root.join(".git/hooks/decoy"),
            "git-decoy",
            "Should not be discovered",
        );
        write_skill(
            &root.join("node_modules/some-pkg"),
            "npm-decoy",
            "Should not be discovered",
        );

        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name.as_str(), "frontend-design");
        assert_eq!(
            skills[0].skill_path.as_str(),
            "skills/frontend-design/SKILL.md"
        );
    }

    #[test]
    fn stops_at_skill_root_but_still_discovers_siblings() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        write_skill(
            &root.join("skills/frontend-design"),
            "frontend-design",
            "Design UI",
        );
        write_skill(
            &root.join("skills/frontend-design/nested"),
            "nested-decoy",
            "Should not be a separate skill",
        );
        write_skill(
            &root.join("skills/backend-review"),
            "backend-review",
            "Review APIs",
        );

        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name.as_str(), "backend-review");
        assert_eq!(
            skills[0].skill_path.as_str(),
            "skills/backend-review/SKILL.md"
        );
        assert_eq!(skills[1].name.as_str(), "frontend-design");
        assert_eq!(
            skills[1].skill_path.as_str(),
            "skills/frontend-design/SKILL.md"
        );
    }
}
