//! 对应 `src/truncate.ts`：工具输出的共享截断工具。
//!
//! 以两个独立上限为准，先到者生效：行数（默认 2000）与字节数（默认 50KB）。
//! **永不返回半行** —— 只输出完整行。
//!
//! 注：上游手写 UTF-8 计数是为了在没有 `Buffer` 的环境下工作；Rust 的 `str` 本身就是
//! UTF-8，字节长度即 `len()`，因此 [`utf8_byte_length`] 是直接的等价实现。

/// 对应 `DEFAULT_MAX_LINES`。
pub const DEFAULT_MAX_LINES: usize = 2000;
/// 对应 `DEFAULT_MAX_BYTES`。
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// 对应 `truncatedBy`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    /// 命中行数上限。
    Lines,
    /// 命中字节上限。
    Bytes,
}

/// 对应 `TruncationResult`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    /// 截断后的内容。
    pub content: String,
    /// 是否发生了截断。
    pub truncated: bool,
    /// 命中了哪个上限（未截断时为 `None`）。
    pub truncated_by: Option<TruncatedBy>,
    /// 原始内容的总行数。
    pub total_lines: usize,
    /// 原始内容的总字节数。
    pub total_bytes: usize,
    /// 输出中完整行的数量。
    pub output_lines: usize,
    /// 输出的字节数。
    pub output_bytes: usize,
    /// 末行是否被部分截断（仅尾部截断的边缘情形）。
    pub last_line_partial: bool,
    /// 首行本身即超出字节上限。
    pub first_line_exceeds_limit: bool,
    /// 生效的行数上限。
    pub max_lines: usize,
    /// 生效的字节上限。
    pub max_bytes: usize,
}

/// 对应 `TruncationOptions`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TruncationOptions {
    /// 最大行数（默认 2000）。
    pub max_lines: Option<usize>,
    /// 最大字节数（默认 50KB）。
    pub max_bytes: Option<usize>,
}

/// 对应 `utf8ByteLength`。
pub fn utf8_byte_length(content: &str) -> usize {
    content.len()
}

/// 对应 `splitLinesForCounting`：按 `\n` 切分，忽略结尾换行带来的空行。
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// 对应 `formatSize`：把字节数格式化为人类可读的大小。
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 对应 `truncateHead`：从头保留前 N 行/字节。
///
/// 适用于「想看到开头」的文件读取。若首行本身超过字节上限，则返回空内容并置
/// `first_line_exceeds_limit`。
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let totals = (
        split_lines_for_counting(content).len(),
        utf8_byte_length(content),
    );
    truncate_head_of(content, totals, options)
}

/// 对应 `truncateHeadOf`：对「已知前缀 + 总量」的文本做同样的截断。
///
/// 前缀必须是完整文本，或长于 `max_bytes + 1` 字节，或至少包含 `max_lines` 个换行；
/// 满足时结果与对全文调用 [`truncate_head`] 一致。
pub fn truncate_head_of(
    prefix: &str,
    totals: (usize, usize),
    options: TruncationOptions,
) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let (total_lines, total_bytes) = totals;
    let lines = split_lines_for_counting(prefix);

    // 无需截断。
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: prefix.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // 首行单独就超过字节上限。
    let first_line_bytes = utf8_byte_length(lines.first().copied().unwrap_or(""));
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    // 收集能完整放下的行。
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (index, line) in lines.iter().enumerate() {
        if index >= max_lines {
            break;
        }
        let line_bytes = utf8_byte_length(line) + usize::from(index > 0); // 换行符占 1 字节
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output.push(line);
        output_bytes_count += line_bytes;
    }

    // 若没有因字节中断，则只有「确实省略了行」才能证明是行数上限；
    // 否则是结尾换行把字节顶超了。
    if truncated_by != TruncatedBy::Bytes {
        truncated_by = if output.len() < total_lines {
            TruncatedBy::Lines
        } else {
            TruncatedBy::Bytes
        };
    }

    let output_content = output.join("\n");
    let final_output_bytes = utf8_byte_length(&output_content);

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output.len(),
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_when_within_limits() {
        let result = truncate_head("a\nb\nc", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.truncated_by, None);
        assert_eq!(result.content, "a\nb\nc");
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn trailing_newline_is_not_counted_as_a_line() {
        assert_eq!(split_lines_for_counting("a\nb\n").len(), 2);
        assert_eq!(split_lines_for_counting("").len(), 0);
        assert_eq!(split_lines_for_counting("\n").len(), 1);
    }

    #[test]
    fn truncates_by_lines() {
        let content = (0..10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head(
            &content,
            TruncationOptions {
                max_lines: Some(3),
                max_bytes: None,
            },
        );
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.content, "0\n1\n2");
        assert_eq!(result.output_lines, 3);
        assert_eq!(result.total_lines, 10);
    }

    #[test]
    fn truncates_by_bytes_without_partial_lines() {
        let content = "aaaa\nbbbb\ncccc";
        let result = truncate_head(
            content,
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(9), // "aaaa" + "\n" + "bbbb" = 9
            },
        );
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "aaaa\nbbbb", "不得返回半行");
        assert_eq!(result.output_bytes, 9);
    }

    #[test]
    fn first_line_over_limit_yields_empty_content() {
        let content = "0123456789\nb";
        let result = truncate_head(
            content,
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(5),
            },
        );
        assert!(result.first_line_exceeds_limit);
        assert_eq!(result.content, "");
        assert_eq!(result.output_lines, 0);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn utf8_byte_length_counts_bytes_not_chars() {
        assert_eq!(utf8_byte_length("abc"), 3);
        assert_eq!(utf8_byte_length("中文"), 6);
        assert_eq!(utf8_byte_length("😀"), 4);
    }

    #[test]
    fn format_size_matches_upstream_units() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn truncate_head_of_matches_truncate_head_on_full_text() {
        let content = "a\nbb\nccc\ndddd";
        for options in [
            TruncationOptions::default(),
            TruncationOptions {
                max_lines: Some(2),
                max_bytes: None,
            },
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(6),
            },
        ] {
            let totals = (
                split_lines_for_counting(content).len(),
                utf8_byte_length(content),
            );
            assert_eq!(
                truncate_head_of(content, totals, options),
                truncate_head(content, options),
                "全文前缀时两者必须一致",
            );
        }
    }
}
