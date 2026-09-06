//! Rust 翻译自 packages/agent/src/harness/prompt-templates.ts
//!
//! 注：TS 的 frontmatter 用 `yaml` 解析；Rust 中简化为手写解析 `description` /
//! `argument-hint` 两个键。

use std::sync::Arc;

use crate::harness::context::Context;
use crate::harness::types::{ExecutionEnv, PromptTemplate};

/// 对应 `PromptTemplateDiagnosticCode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTemplateDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
}

impl PromptTemplateDiagnosticCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileInfoFailed => "file_info_failed",
            Self::ListFailed => "list_failed",
            Self::ReadFailed => "read_failed",
            Self::ParseFailed => "parse_failed",
        }
    }
}

/// 对应 `PromptTemplateDiagnostic`
#[derive(Debug, Clone)]
pub struct PromptTemplateDiagnostic {
    pub code: PromptTemplateDiagnosticCode,
    pub message: String,
    pub path: String,
}

/// 对应 `loadPromptTemplates`
pub async fn load_prompt_templates(
    env: &Arc<dyn ExecutionEnv>,
    paths: &[String],
    context: &Context,
) -> (Vec<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut templates = Vec::new();
    let diagnostics = Vec::new();
    for path in paths {
        let info = match env.file_info(path, context).await {
            Ok(info) => info,
            Err(_) => continue,
        };
        if info.kind == crate::harness::types::FileKind::Directory {
            let entries = env.list_dir(&info.path, context).await.unwrap_or_default();
            let mut sorted = entries;
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for entry in sorted {
                if entry.kind == crate::harness::types::FileKind::File
                    && entry.name.ends_with(".md")
                    && let Ok(content) = env.read_text_file(&entry.path, context).await
                    && let Some(template) = parse_template_file(&content, &entry.name)
                {
                    templates.push(template);
                }
            }
        } else if info.kind == crate::harness::types::FileKind::File
            && info.name.ends_with(".md")
            && let Ok(content) = env.read_text_file(&info.path, context).await
            && let Some(template) = parse_template_file(&content, &info.name)
        {
            templates.push(template);
        }
    }
    (templates, diagnostics)
}

/// 对应 `loadSourcedPromptTemplates`：从带 source 的路径加载，source 附加到每个结果。
#[allow(clippy::type_complexity)]
pub async fn load_sourced_prompt_templates<T: Clone>(
    env: &Arc<dyn ExecutionEnv>,
    inputs: &[(String, T)],
    map_prompt_template: Option<&(dyn Fn(&PromptTemplate, &T) -> PromptTemplate + Sync)>,
    context: &Context,
) -> (Vec<(PromptTemplate, T)>, Vec<(PromptTemplateDiagnostic, T)>) {
    let mut prompt_templates = Vec::new();
    let mut diagnostics = Vec::new();
    for (path, source) in inputs {
        let (templates, diags) =
            load_prompt_templates(env, std::slice::from_ref(path), context).await;
        for template in templates {
            let mapped = match map_prompt_template {
                Some(map) => map(&template, source),
                None => template,
            };
            prompt_templates.push((mapped, source.clone()));
        }
        for diagnostic in diags {
            diagnostics.push((diagnostic, source.clone()));
        }
    }
    (prompt_templates, diagnostics)
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
    let all_args = args.join(" ");
    let mut result = String::new();
    let mut i = 0;
    while i < content.len() {
        let rest = &content[i..];
        // ${@:N} 或 ${@:N:L}
        if rest.starts_with("${@:")
            && let Some(close) = rest.find('}')
        {
            let inner = &rest[4..close];
            let mut parts = inner.split(':');
            let start = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
                .saturating_sub(1);
            let len = parts.next().and_then(|s| s.parse::<usize>().ok());
            let slice: Vec<&str> = match len {
                Some(l) => args
                    .iter()
                    .skip(start)
                    .take(l)
                    .map(|s| s.as_str())
                    .collect(),
                None => args.iter().skip(start).map(|s| s.as_str()).collect(),
            };
            result.push_str(&slice.join(" "));
            i += close + 1;
            continue;
        }
        // $ARGUMENTS
        if rest.starts_with("$ARGUMENTS") {
            result.push_str(&all_args);
            i += "$ARGUMENTS".len();
            continue;
        }
        // $@
        if rest.starts_with("$@") {
            result.push_str(&all_args);
            i += 2;
            continue;
        }
        // $N（占位符）
        if rest.starts_with('$')
            && let Some(first) = rest[1..].chars().next()
            && first.is_ascii_digit()
        {
            let mut num = String::new();
            let mut j = i + 1;
            for ch in content[j..].chars() {
                if ch.is_ascii_digit() {
                    num.push(ch);
                    j += ch.len_utf8();
                } else {
                    break;
                }
            }
            let index: usize = num.parse().unwrap_or(1);
            result.push_str(
                args.get(index.saturating_sub(1))
                    .map(|s| s.as_str())
                    .unwrap_or(""),
            );
            i = j;
            continue;
        }
        // 普通字符（按 UTF-8 边界推进）
        let ch = rest.chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

/// 对应 `formatPromptTemplateInvocation`
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &[String]) -> String {
    substitute_args(&template.content, args)
}
