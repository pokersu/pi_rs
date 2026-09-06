//! Rust 翻译自 packages/agent/src/harness/skills.ts
//!
//! 注：TS 依赖 `ignore` 包做 gitignore 风格匹配；Rust 中省略 ignore 规则（仅跳过
//! `.` 开头目录与 `node_modules`），核心的 SKILL.md 加载与元数据校验完整保留。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::harness::context::Context;
use crate::harness::types::{ExecutionEnv, Skill};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// 对应 `SkillDiagnosticCode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
    InvalidMetadata,
}

impl SkillDiagnosticCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileInfoFailed => "file_info_failed",
            Self::ListFailed => "list_failed",
            Self::ReadFailed => "read_failed",
            Self::ParseFailed => "parse_failed",
            Self::InvalidMetadata => "invalid_metadata",
        }
    }
}

/// 对应 `SkillDiagnostic`
#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    pub code: SkillDiagnosticCode,
    pub message: String,
    pub path: String,
}

/// 对应 `formatSkillInvocation`
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path,
        dirname_env_path(&skill.file_path),
        skill.content
    );
    match additional_instructions {
        Some(instructions) => format!("{skill_block}\n\n{instructions}"),
        None => skill_block,
    }
}

/// 对应 `loadSkills`
pub async fn load_skills(
    env: &Arc<dyn ExecutionEnv>,
    dirs: &[String],
    context: &Context,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for dir in dirs {
        if let Ok(info) = env.file_info(dir, context).await
            && info.kind == crate::harness::types::FileKind::Directory
        {
            load_skills_from_dir(
                env,
                &info.path,
                true,
                &info.path,
                &mut skills,
                &mut diagnostics,
                context,
            )
            .await;
        }
    }
    (skills, diagnostics)
}

/// 对应 `loadSourcedSkills`：从带 source 的路径加载，source 附加到每个结果。
#[allow(clippy::type_complexity)]
pub async fn load_sourced_skills<T: Clone>(
    env: &Arc<dyn ExecutionEnv>,
    inputs: &[(String, T)],
    map_skill: Option<&(dyn Fn(&Skill, &T) -> Skill + Sync)>,
    context: &Context,
) -> (Vec<(Skill, T)>, Vec<(SkillDiagnostic, T)>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for (path, source) in inputs {
        let (loaded, diags) = load_skills(env, std::slice::from_ref(path), context).await;
        for skill in loaded {
            let mapped = match map_skill {
                Some(map) => map(&skill, source),
                None => skill,
            };
            skills.push((mapped, source.clone()));
        }
        for diagnostic in diags {
            diagnostics.push((diagnostic, source.clone()));
        }
    }
    (skills, diagnostics)
}

async fn load_skills_from_dir(
    env: &Arc<dyn ExecutionEnv>,
    dir: &str,
    include_root_files: bool,
    _root_dir: &str,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
    context: &Context,
) {
    load_skills_from_dir_inner(
        env,
        dir,
        include_root_files,
        _root_dir,
        skills,
        diagnostics,
        context,
    )
    .await
}

fn load_skills_from_dir_inner<'a>(
    env: &'a Arc<dyn ExecutionEnv>,
    dir: &'a str,
    include_root_files: bool,
    _root_dir: &'a str,
    skills: &'a mut Vec<Skill>,
    diagnostics: &'a mut Vec<SkillDiagnostic>,
    context: &'a Context,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let entries = match env.list_dir(dir, context).await {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in &entries {
            if entry.name != "SKILL.md" || entry.kind != crate::harness::types::FileKind::File {
                continue;
            }
            let parent_name = std::path::Path::new(dir)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(skill) =
                load_skill_from_file(env, &entry.path, &parent_name, diagnostics, context).await
            {
                skills.push(skill);
            }
            return;
        }

        let mut sorted = entries;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in sorted {
            if entry.name.starts_with('.') || entry.name == "node_modules" {
                continue;
            }
            match entry.kind {
                crate::harness::types::FileKind::Directory => {
                    load_skills_from_dir_inner(
                        env,
                        &entry.path,
                        false,
                        _root_dir,
                        skills,
                        diagnostics,
                        context,
                    )
                    .await;
                }
                crate::harness::types::FileKind::File => {
                    if !include_root_files || !entry.name.ends_with(".md") {
                        continue;
                    }
                    let parent_name = std::path::Path::new(dir)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if let Some(skill) =
                        load_skill_from_file(env, &entry.path, &parent_name, diagnostics, context)
                            .await
                    {
                        skills.push(skill);
                    }
                }
                _ => {}
            }
        }
    })
}

async fn load_skill_from_file(
    env: &Arc<dyn ExecutionEnv>,
    file_path: &str,
    parent_dir_name: &str,
    diagnostics: &mut Vec<SkillDiagnostic>,
    context: &Context,
) -> Option<Skill> {
    let is_declared_skill =
        file_path.trim_end_matches('/').split('/').next_back() == Some("SKILL.md");
    let raw_content = match env.read_text_file(file_path, context).await {
        Ok(content) => content,
        Err(e) => {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::ReadFailed,
                message: e.message,
                path: file_path.into(),
            });
            return None;
        }
    };

    let (frontmatter, body) = parse_frontmatter(&raw_content);
    let description = frontmatter.get("description").cloned().unwrap_or_default();
    if !is_declared_skill && description.trim().is_empty() {
        return None;
    }

    let frontmatter_name = frontmatter.get("name").cloned();
    let name = frontmatter_name.unwrap_or_else(|| parent_dir_name.to_string());
    for error in validate_name(&name, parent_dir_name) {
        diagnostics.push(SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message: error,
            path: file_path.into(),
        });
    }
    for error in validate_description(Some(&description)) {
        diagnostics.push(SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message: error,
            path: file_path.into(),
        });
    }
    if description.trim().is_empty() {
        return None;
    }

    Some(Skill {
        name,
        description,
        content: body,
        file_path: file_path.to_string(),
        disable_model_invocation: frontmatter
            .get("disable-model-invocation")
            .map(|v| v == "true")
            .unwrap_or(false),
    })
}

/// 对应 `parseFrontmatter`（简化：仅解析 `key: value` 行）。
fn parse_frontmatter(content: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut frontmatter = std::collections::BTreeMap::new();
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (frontmatter, normalized);
    }
    let Some(rel_end) = normalized[3..].find("\n---") else {
        return (frontmatter, normalized);
    };
    let end_index = rel_end + 3;
    let yaml_string = &normalized[3..end_index];
    let body = normalized[end_index + 4..].trim().to_string();
    for line in yaml_string.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            frontmatter.insert(key.trim().to_string(), value);
        }
    }
    (frontmatter, body)
}

/// 对应 `validateName`
fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

/// 对应 `validateDescription`
fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        None => errors.push("description is required".to_string()),
        Some(d) if d.trim().is_empty() => errors.push("description is required".to_string()),
        Some(d) if d.len() > MAX_DESCRIPTION_LENGTH => {
            errors.push(format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                d.len()
            ));
        }
        _ => {}
    }
    errors
}

/// 对应 `dirnameEnvPath`
pub fn dirname_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches('/').trim_end_matches('\\');
    let separator_index = normalized.rfind('/').or_else(|| normalized.rfind('\\'));
    match separator_index {
        Some(i) if i == 2 && normalized.as_bytes().get(1) == Some(&b':') => {
            normalized[..3].to_string()
        }
        Some(i) => normalized[..i].to_string(),
        None => "/".to_string(),
    }
}

/// 对应 `relativeEnvPath`
pub fn relative_env_path(root: &str, path: &str) -> String {
    let normalized_root = root.replace('\\', "/").trim_end_matches('/').to_string();
    let normalized_path = path.replace('\\', "/").trim_end_matches('/').to_string();
    if normalized_path == normalized_root {
        return String::new();
    }
    if let Some(stripped) = normalized_path.strip_prefix(&format!("{normalized_root}/")) {
        stripped.to_string()
    } else {
        normalized_path.trim_start_matches('/').to_string()
    }
}
