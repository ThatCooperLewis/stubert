use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub allowed_tools: Option<Vec<String>>,
    pub add_dirs: Option<Vec<String>>,
    pub file_path: PathBuf,
}

pub struct SkillRegistry {
    skills: HashMap<String, SkillInfo>,
    skills_dir: PathBuf,
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    allowed_tools: Option<Vec<String>>,
    add_dirs: Option<Vec<String>>,
}

impl SkillRegistry {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self {
            skills: HashMap::new(),
            skills_dir,
        }
    }

    pub fn discover(&mut self) {
        let entries = match fs::read_dir(&self.skills_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to read skill file"
                    );
                    continue;
                }
            };

            match Self::parse_skill(&content, &path) {
                Some(skill) => {
                    self.skills.insert(skill.name.clone(), skill);
                }
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "skill file skipped: missing or invalid frontmatter"
                    );
                }
            }
        }
    }

    fn parse_skill(content: &str, file_path: &Path) -> Option<SkillInfo> {
        if !content.starts_with("---") {
            return None;
        }

        let rest = &content[3..];
        let end_idx = rest.find("\n---")?;
        let frontmatter_str = &rest[..end_idx];

        let fm: SkillFrontmatter = serde_yaml_ng::from_str(frontmatter_str).ok()?;
        let name = fm.name?;

        Some(SkillInfo {
            name,
            description: fm.description.unwrap_or_default(),
            allowed_tools: fm.allowed_tools,
            add_dirs: fm.add_dirs,
            file_path: file_path.to_path_buf(),
        })
    }

    pub fn get(&self, name: &str) -> Option<&SkillInfo> {
        self.skills.get(name)
    }

    pub fn list_skills(&self) -> Vec<&SkillInfo> {
        let mut skills: Vec<&SkillInfo> = self.skills.values().collect();
        skills.sort_by_key(|s| &s.name);
        skills
    }

    pub fn read_prompt(&self, name: &str) -> Option<String> {
        let skill = self.skills.get(name)?;
        let content = fs::read_to_string(&skill.file_path).ok()?;

        if !content.starts_with("---") {
            return None;
        }
        let rest = &content[3..];
        let end_idx = rest.find("\n---")?;
        let body = &rest[end_idx + 4..]; // skip "\n---"
        let body = body.strip_prefix('\n').unwrap_or(body);
        Some(body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill_file(dir: &Path, filename: &str, content: &str) {
        fs::write(dir.join(filename), content).unwrap();
    }

    fn valid_skill() -> String {
        r#"---
name: trello
description: Manage Trello boards
allowed_tools:
  - Bash
  - Read
add_dirs:
  - /extra/dir
---
Create a Trello card with the given details."#
            .to_string()
    }

    fn minimal_skill() -> String {
        r#"---
name: simple
description: A simple skill
---
Do the thing."#
            .to_string()
    }

    mod test_discover {
        use super::*;

        #[test]
        fn valid_frontmatter_with_all_fields() {
            let dir = TempDir::new().unwrap();
            make_skill_file(dir.path(), "trello.md", &valid_skill());

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            let skill = registry.get("trello").unwrap();
            assert_eq!(skill.name, "trello");
            assert_eq!(skill.description, "Manage Trello boards");
            assert_eq!(
                skill.allowed_tools.as_ref().unwrap(),
                &vec!["Bash".to_string(), "Read".to_string()]
            );
            assert_eq!(
                skill.add_dirs.as_ref().unwrap(),
                &vec!["/extra/dir".to_string()]
            );
        }

        #[test]
        fn missing_name_field_skipped() {
            let dir = TempDir::new().unwrap();
            let content = "---\ndescription: no name\n---\nbody";
            make_skill_file(dir.path(), "bad.md", content);

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            assert!(registry.list_skills().is_empty());
        }

        #[test]
        fn no_frontmatter_delimiters_skipped() {
            let dir = TempDir::new().unwrap();
            make_skill_file(dir.path(), "plain.md", "Just some text without frontmatter.");

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            assert!(registry.list_skills().is_empty());
        }

        #[test]
        fn list_skills_returns_all_discovered() {
            let dir = TempDir::new().unwrap();
            make_skill_file(dir.path(), "trello.md", &valid_skill());
            make_skill_file(dir.path(), "simple.md", &minimal_skill());

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            let skills = registry.list_skills();
            assert_eq!(skills.len(), 2);
            // Sorted by name
            assert_eq!(skills[0].name, "simple");
            assert_eq!(skills[1].name, "trello");
        }

        #[test]
        fn get_returns_none_for_unknown() {
            let dir = TempDir::new().unwrap();
            let registry = SkillRegistry::new(dir.path().to_path_buf());
            assert!(registry.get("nonexistent").is_none());
        }

        #[test]
        fn missing_skills_directory_no_panic() {
            let dir = TempDir::new().unwrap();
            let missing = dir.path().join("nonexistent");

            let mut registry = SkillRegistry::new(missing);
            registry.discover(); // should not panic

            assert!(registry.list_skills().is_empty());
        }

        #[test]
        fn allowed_tools_and_add_dirs_optional() {
            let dir = TempDir::new().unwrap();
            make_skill_file(dir.path(), "simple.md", &minimal_skill());

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            let skill = registry.get("simple").unwrap();
            assert!(skill.allowed_tools.is_none());
            assert!(skill.add_dirs.is_none());
        }
    }

    mod test_read_prompt {
        use super::*;

        #[test]
        fn returns_body_only() {
            let dir = TempDir::new().unwrap();
            make_skill_file(dir.path(), "trello.md", &valid_skill());

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            let prompt = registry.read_prompt("trello").unwrap();
            assert_eq!(prompt, "Create a Trello card with the given details.");
            assert!(!prompt.contains("---"));
            assert!(!prompt.contains("name:"));
        }

        #[test]
        fn only_frontmatter_returns_empty() {
            let dir = TempDir::new().unwrap();
            let content = "---\nname: empty\ndescription: nothing\n---\n";
            make_skill_file(dir.path(), "empty.md", content);

            let mut registry = SkillRegistry::new(dir.path().to_path_buf());
            registry.discover();

            let prompt = registry.read_prompt("empty").unwrap();
            assert_eq!(prompt, "");
        }

        #[test]
        fn unknown_skill_returns_none() {
            let dir = TempDir::new().unwrap();
            let registry = SkillRegistry::new(dir.path().to_path_buf());
            assert!(registry.read_prompt("nope").is_none());
        }
    }
}
