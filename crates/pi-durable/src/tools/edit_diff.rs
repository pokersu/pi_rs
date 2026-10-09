//! 对应 `tools/edit-diff.ts`：edit 工具的 diff / 补丁 / 模糊匹配工具。

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// 对应 `detectLineEnding`。
pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    if lf.is_none() {
        return "\n";
    }
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

/// 对应 `normalizeToLF`。
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 对应 `restoreLineEndings`。
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

static SMART_SINGLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[\u{2018}\u{2019}\u{201A}\u{201B}]").expect("smart single"));
static SMART_DOUBLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[\u{201C}\u{201D}\u{201E}\u{201F}]").expect("smart double"));
static DASHES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{2010}\u{2011}\u{2012}\u{2013}\u{2014}\u{2015}\u{2212}]").expect("dashes")
});
static SPECIAL_SPACES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{00A0}\u{2002}-\u{200A}\u{202F}\u{205F}\u{3000}]").expect("spaces")
});

/// 对应 `normalizeForFuzzyMatch`。
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc = text.nfkc().collect::<String>();
    let trimmed = nfkc
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let step = SMART_SINGLE.replace_all(&trimmed, "'");
    let step = SMART_DOUBLE.replace_all(&step, "\"");
    let step = DASHES.replace_all(&step, "-");
    SPECIAL_SPACES.replace_all(&step, " ").to_string()
}

/// 对应 `splitLinesWithEndings`。
pub fn split_lines_with_endings(content: &str) -> Vec<String> {
    static LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new("[^\n]*\n|[^\n]+").expect("lines"));
    LINES
        .find_iter(content)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// 对应 `LineSpan`。
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
}

/// 对应 `TextReplacement`。
#[derive(Debug, Clone)]
struct TextReplacement {
    match_index: usize,
    match_length: usize,
    new_text: String,
}

fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .iter()
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

/// 对应 `applyReplacementsPreservingUnchangedLines`。
fn apply_replacements_preserving_unchanged_lines(
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
    sorted.sort_by_key(|replacement| replacement.match_index);
    for replacement in sorted {
        let (start_line, end_line) = get_replacement_line_range(&base_lines, &replacement);
        if let Some((_group_start, group_end, group_replacements)) = groups.last_mut()
            && start_line < *group_end
        {
            *group_end = (*group_end).max(end_line);
            group_replacements.push(replacement);
            continue;
        }
        groups.push((start_line, end_line, vec![replacement]));
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (start_line, end_line, group_replacements) in groups {
        result.push_str(&original_lines[original_line_index..start_line].concat());
        let group_start_offset = base_lines[start_line].start;
        let group_end_offset = base_lines[end_line - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &group_replacements,
            group_start_offset,
        ));
        original_line_index = end_line;
    }
    result.push_str(&original_lines[original_line_index..].concat());
    result
}

/// 对应 `FuzzyMatchResult`。
#[derive(Debug, Clone)]
pub struct FuzzyMatchResult {
    pub found: bool,
    pub index: usize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    pub content_for_replacement: String,
}

/// 对应 `fuzzyFindText`。
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
    if let Some(fuzzy_index) = fuzzy_content.find(&fuzzy_old_text) {
        return FuzzyMatchResult {
            found: true,
            index: fuzzy_index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        };
    }
    FuzzyMatchResult {
        found: false,
        index: 0,
        match_length: 0,
        used_fuzzy_match: false,
        content_for_replacement: content.to_string(),
    }
}

/// 对应 `stripBom`。
pub fn strip_bom(content: &str) -> (&str, &str) {
    content
        .strip_prefix('\u{FEFF}')
        .map_or(("", content), |text| ("\u{FEFF}", text))
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content.matches(&fuzzy_old_text).count()
}

/// 对应 `Edit`。
#[derive(Debug, Clone)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// 对应 `AppliedEditsResult`。
#[derive(Debug, Clone)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

fn get_not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn get_duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn get_empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn get_no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// 对应 `MatchedEdit`。
#[derive(Debug, Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

/// 对应 `applyEditsToNormalizedContent`。
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(get_empty_old_text_error(
                path,
                index,
                normalized_edits.len(),
            ));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|edit| fuzzy_find_text(normalized_content, &edit.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (index, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(get_not_found_error(path, index, normalized_edits.len()));
        }
        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(get_duplicate_error(
                path,
                index,
                normalized_edits.len(),
                occurrences,
            ));
        }
        matched_edits.push(MatchedEdit {
            edit_index: index,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|edit| edit.match_index);
    for window in matched_edits.windows(2) {
        let previous = &window[0];
        let current = &window[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let replacements: Vec<TextReplacement> = matched_edits
        .iter()
        .map(|edit| TextReplacement {
            match_index: edit.match_index,
            match_length: edit.match_length,
            new_text: edit.new_text.clone(),
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
        return Err(get_no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

/// 对应 `generateUnifiedPatch`。
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    similar::TextDiff::from_lines(old_content, new_content)
        .unified_diff()
        .header(path, path)
        .context_radius(context_lines)
        .to_string()
}

/// 对应 `generateDiffString`。
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let diff = similar::TextDiff::from_lines(old_content, new_content);
    let changes: Vec<(similar::ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|change| (change.tag(), change.value().to_string()))
        .collect();
    let mut output: Vec<String> = Vec::new();

    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (index, (tag, value)) in changes.iter().enumerate() {
        let raw: Vec<&str> = value.split('\n').collect();
        let raw: Vec<&str> = if raw.last() == Some(&"") {
            raw[..raw.len() - 1].to_vec()
        } else {
            raw
        };
        let is_change = *tag != similar::ChangeTag::Equal;

        if is_change {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            for line in &raw {
                if *tag == similar::ChangeTag::Insert {
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
            let has_leading_change = last_was_change;
            let next_tag = changes.get(index + 1).map(|(tag, _)| *tag);
            let has_trailing_change = next_tag == Some(similar::ChangeTag::Delete)
                || next_tag == Some(similar::ChangeTag::Insert);

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
                let shown = &raw[..raw.len().min(context_lines)];
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
