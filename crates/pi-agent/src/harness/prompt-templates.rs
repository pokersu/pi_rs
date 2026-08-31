//! Rust 翻译自 packages/agent/src/harness/prompt-templates.ts
//!
//! 注：TS 的 frontmatter 用 `yaml` 解析；Rust 中简化为手写解析 `description` /
//! `argument-hint` 两个键。

use std::sync::Arc;

use crate::harness::types::{ExecutionEnv, PromptTemplate};

/// 对应 `PromptTemplateDiagnostic`
#[derive(Debug, Clone)]
pub struct PromptTemplateDiagnostic {
    pub code: String,
    pub message: String,
    pub path: String,
}

/// 对应 `loadPromptTemplates`
pub async fn load_prompt_templates(
    env: &Arc<dyn ExecutionEnv>,
    paths: &[String],
) -> (Vec<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut templates = Vec::new();
    let diagnostics = Vec::new();
    for path in paths {
        let info = match env.file_info(path, None).await {
            Ok(info) => info,
            Err(_) => continue,
        };
        if info.kind == crate::harness::types::FileKind::Directory {
            let entries = env.list_dir(&info.path, None).await.unwrap_or_default();
            let mut sorted = entries;
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for entry in sorted {
                if entry.kind == crate::harness::types::FileKind::File
                    && entry.name.ends_with(".md")
                    && let Ok(content) = env.read_text_file(&entry.path, None).await
                    && let Some(template) = parse_template_file(&content, &entry.name)
                {
                    templates.push(template);
                }
            }
        } else if info.kind == crate::harness::types::FileKind::File
            && info.name.ends_with(".md")
            && let Ok(content) = env.read_text_file(&info.path, None).await
            && let Some(template) = parse_template_file(&content, &info.name)
        {
            templates.push(template);
        }
    }
    (templates, diagnostics)
}

fn parse_template_file(content: &str, file_name: &str) -> Option<PromptTemplate> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let (frontmatter, body) = parse_frontmatter(&normalized);
    let description = frontmatter.get("description").cloned().unwrap_or_default();
    let description = if description.is_empty() {
        body.lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| {
                let mut d = l.trim().to_string();
                if d.len() > 60 {
                    d = d[..60].to_string() + "...";
                }
                d
            })
            .unwrap_or_default()
    } else {
        description
    };
    Some(PromptTemplate {
        name: file_name.trim_end_matches(".md").to_string(),
        description: Some(description),
        content: body,
    })
}

/// 对应 `parseFrontmatter`（简化：仅解析 `key: value` 行）。
fn parse_frontmatter(content: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut frontmatter = std::collections::BTreeMap::new();
    if !content.starts_with("---") {
        return (frontmatter, content.to_string());
    }
    let Some(end_index) = content[3..].find("\n---") else {
        return (frontmatter, content.to_string());
    };
    let end_index = end_index + 3;
    let yaml_string = &content[3..end_index];
    let body = content[end_index + 4..].trim().to_string();
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

/// 对应 `parseCommandArgs`
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for c in args_string.chars() {
        if let Some(quote) = in_quote {
            if c == quote {
                in_quote = None;
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            in_quote = Some(c);
        } else if c == ' ' || c == '\t' {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// 对应 `substituteArgs`
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut result = content.to_string();

    // ${@:N} 和 ${@:N:L}
    // 简化：处理 $ARGUMENTS 和 $@ 与 $N
    result = result.replace("$ARGUMENTS", &args.join(" "));
    result = result.replace("$@", &args.join(" "));

    // $N（占位符）
    let mut substituted = String::new();
    let mut chars = result.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$'
            && let Some(&next) = chars.peek()
            && next.is_ascii_digit()
        {
            let mut num = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            let index: usize = num.parse().unwrap_or(1);
            substituted.push_str(
                args.get(index.saturating_sub(1))
                    .map(|s| s.as_str())
                    .unwrap_or(""),
            );
            continue;
        }
        substituted.push(c);
    }
    substituted
}

/// 对应 `formatPromptTemplateInvocation`
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &[String]) -> String {
    substitute_args(&template.content, args)
}
