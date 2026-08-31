//! Rust 翻译自 packages/agent/src/harness/tools/edit-diff.ts
//!
//! edit 工具的共享 diff 计算工具。注：TS 的 `normalize("NFKC")` 归一化在 Rust 中
//! 未引入 `unicode-normalization` 依赖，此处仅做 Unicode 字符替换（引号/破折号/空格）。

use similar::{ChangeTag, TextDiff};

/// 对应 `detectLineEnding`
pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    match (lf, crlf) {
        (None, _) => "\n",
        (_, None) => "\n",
        (Some(lf), Some(crlf)) => {
            if crlf < lf {
                "\r\n"
            } else {
                "\n"
            }
        }
    }
}

/// 对应 `normalizeToLF`
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 对应 `restoreLineEndings`
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// 对应 `normalizeForFuzzyMatch`（省略 NFKC）。
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let mut result = String::new();
    for line in text.split('\n') {
        result.push_str(line.trim_end());
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

fn split_lines_with_endings(content: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for c in content.chars() {
        current.push(c);
        if c == '\n' {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[derive(Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
}

#[derive(Clone)]
pub struct TextReplacement {
    match_index: usize,
    match_length: usize,
    new_text: String,
}

#[derive(Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

fn get_replacement_line_range(lines: &[LineSpan], replacement: &TextReplacement) -> (usize, usize) {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let start_line = lines
        .iter()
        .position(|line| replacement_start >= line.start && replacement_start < line.end)
        .expect("Replacement range is outside the base content.");

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        panic!("Replacement range is outside the base content.");
    }

    (start_line, end_line + 1)
}

fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index - offset;
        result = format!(
            "{}{}{}",
            &result[..match_index],
            replacement.new_text,
            &result[match_index + replacement.match_length..]
        );
    }
    result
}

/// 对应 `applyReplacementsPreservingUnchangedLines`
pub fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> String {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        panic!(
            "Cannot preserve unchanged lines because the base content has a different line count."
        );
    }

    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|r| r.match_index);
    for replacement in sorted {
        let (start_line, end_line) = get_replacement_line_range(&base_lines, &replacement);
        if let Some(last) = groups.last_mut()
            && start_line < last.1
        {
            last.1 = last.1.max(end_line);
            last.2.push(replacement);
            continue;
        }
        groups.push((start_line, end_line, vec![replacement]));
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (start_line, end_line, group_replacements) in &groups {
        result.push_str(&original_lines[original_line_index..*start_line].join(""));

        let group_start_offset = base_lines[*start_line].start;
        let group_end_offset = base_lines[*end_line - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            group_replacements,
            group_start_offset,
        ));
        original_line_index = *end_line;
    }
    result.push_str(&original_lines[original_line_index..].join(""));

    result
}

/// 对应 `FuzzyMatchResult`
#[derive(Debug, Clone)]
pub struct FuzzyMatchResult {
    pub found: bool,
    pub index: usize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    pub content_for_replacement: String,
}

/// 对应 `fuzzyFindText`
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(exact_index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: exact_index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        Some(fuzzy_index) => FuzzyMatchResult {
            found: true,
            index: fuzzy_index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
        None => FuzzyMatchResult {
            found: false,
            index: 0,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        },
    }
}

/// 对应 `stripBom`
pub fn strip_bom(content: &str) -> (String, String) {
    if let Some(stripped) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), stripped.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content.matches(&fuzzy_old_text).count()
}

/// 对应 `Edit`
#[derive(Debug, Clone)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// 对应 `AppliedEditsResult`
#[derive(Debug, Clone)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

/// 对应 `applyEditsToNormalizedContent`
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> AppliedEditsResult {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            if normalized_edits.len() == 1 {
                panic!("oldText must not be empty in {path}.");
            }
            panic!("edits[{i}].oldText must not be empty in {path}.");
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|e| fuzzy_find_text(normalized_content, &e.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            if normalized_edits.len() == 1 {
                panic!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                );
            }
            panic!(
                "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
            );
        }

        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            if normalized_edits.len() == 1 {
                panic!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                );
            }
            panic!(
                "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
            );
        }

        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|m| m.match_index);
    for pair in matched_edits.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.match_index + previous.match_length > current.match_index {
            panic!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            );
        }
    }

    let base_content = normalized_content.to_string();
    let replacements: Vec<TextReplacement> = matched_edits
        .iter()
        .map(|m| TextReplacement {
            match_index: m.match_index,
            match_length: m.match_length,
            new_text: m.new_text.clone(),
        })
        .collect();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &replacements,
        )
    } else {
        apply_replacements(&replacement_base_content, &replacements, 0)
    };

    if base_content == new_content {
        if normalized_edits.len() == 1 {
            panic!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            );
        }
        panic!("No changes made to {path}. The replacements produced identical content.");
    }

    AppliedEditsResult {
        base_content,
        new_content,
    }
}

/// 对应 `generateUnifiedPatch`
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    TextDiff::from_lines(old_content, new_content)
        .unified_diff()
        .context_radius(context_lines)
        .header(path, path)
        .to_string()
}

/// 对应 `generateDiffString`
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let diff = TextDiff::from_lines(old_content, new_content);
    let mut output: Vec<String> = Vec::new();

    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    // 收集 changes 为 (tag, value)。
    let changes: Vec<(ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.value().to_string()))
        .collect();

    for i in 0..changes.len() {
        let (tag, value) = &changes[i];
        let mut raw: Vec<&str> = value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        let added = *tag == ChangeTag::Insert;
        let removed = *tag == ChangeTag::Delete;
        if added || removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            for line in &raw {
                if added {
                    let line_num = format!("{new_line_num:>line_num_width$}");
                    output.push(format!("+{line_num} {line}"));
                    new_line_num += 1;
                } else {
                    let line_num = format!("{old_line_num:>line_num_width$}");
                    output.push(format!("-{line_num} {line}"));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_is_change = i + 1 < changes.len()
                && (changes[i + 1].0 == ChangeTag::Insert || changes[i + 1].0 == ChangeTag::Delete);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in &raw {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading = &raw[..context_lines];
                    let trailing = &raw[raw.len() - context_lines..];
                    let skipped = raw.len() - leading.len() - trailing.len();
                    for line in leading {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped;
                    new_line_num += skipped;
                    for line in trailing {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown = &raw[..context_lines.min(raw.len())];
                let skipped = raw.len() - shown.len();
                for line in shown {
                    let line_num = format!("{old_line_num:>line_num_width$}");
                    output.push(format!(" {line_num} {line}"));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                if skipped > 0 {
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
            } else if has_trailing_change {
                let skipped = raw.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
                for line in &raw[skipped..] {
                    let line_num = format!("{old_line_num:>line_num_width$}");
                    output.push(format!(" {line_num} {line}"));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    (output.join("\n"), first_changed_line)
}
