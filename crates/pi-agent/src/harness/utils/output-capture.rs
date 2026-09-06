//! Rust 翻译自 packages/agent/src/harness/utils/output-capture.ts
//!
//! 维护并发布一个有界 shell 输出视图。限速期间收到的写入折叠进最新视图；小改动保持
//! 响应，整窗口翻转换取更长延迟；空闲后的首个更新与显式最终 flush 立即发布。

use std::sync::{Arc, Mutex};

use crate::harness::context::Context;
use crate::harness::types::{
    ShellOutputCaptureOptions, ShellOutputMetadata, ShellOutputRetention, ShellOutputTruncation,
    ShellOutputUpdate, ShellOutputView,
};
use crate::harness::utils::adaptive_publisher::{AdaptivePublisher, AdaptivePublisherSink};
use crate::harness::utils::truncate::{
    truncate_head, truncate_tail, TruncationOptions, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// 对应 `OUTPUT_MIN_EMIT_INTERVAL_MS`。
pub const OUTPUT_MIN_EMIT_INTERVAL_MS: u64 = 100;
/// 对应 `OUTPUT_TARGET_BYTES_PER_SECOND`。
pub const OUTPUT_TARGET_BYTES_PER_SECOND: u64 = 100 * 1024;

fn is_invalid_shell_output_char(c: char) -> bool {
    let code = c as u32;
    code <= 0x08 || (0x0b..=0x1f).contains(&code) || (0xfff9..=0xfffb).contains(&code)
}

/// 对应 `sanitizeShellOutput`。
pub fn sanitize_shell_output(text: &str) -> String {
    text.chars().filter(|c| !is_invalid_shell_output_char(*c)).collect()
}

fn count_newlines(text: &str) -> usize {
    text.matches('\n').count()
}

/// 对应 `trimToLastUtf8Bytes`：保留最后 `max_bytes` 字节，对齐 UTF-8 边界。
fn trim_to_last_utf8_bytes(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// 对应 `trimToFirstUtf8Bytes`：保留前 `max_bytes` 字节，对齐 UTF-8 边界。
fn trim_to_first_utf8_bytes(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && (bytes[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 对应 `onUpdate` 回调。
pub type ShellOutputUpdateCallback = Arc<dyn Fn(&ShellOutputUpdate, &Context) + Send + Sync>;

struct OutputCaptureState {
    buffer: String,
    buffer_bytes: usize,
    total_bytes: usize,
    newlines: usize,
    ends_with_newline: bool,
    current_line_bytes: usize,
    spill_path: Option<String>,
    disposed: bool,
}

struct CaptureSink {
    state: Arc<Mutex<OutputCaptureState>>,
    max_bytes: usize,
    max_lines: usize,
    retain: ShellOutputRetention,
    context: Context,
    on_update: Option<ShellOutputUpdateCallback>,
}

impl AdaptivePublisherSink<ShellOutputView, ShellOutputUpdate> for CaptureSink {
    fn snapshot(&self) -> ShellOutputView {
        let state = self.state.lock().unwrap();
        snapshot_inner(&state, self.max_bytes, self.max_lines, self.retain)
    }

    fn update(
        &self,
        previous: Option<&ShellOutputView>,
        current: &ShellOutputView,
    ) -> Option<ShellOutputUpdate> {
        update_from(previous, current)
    }

    fn measure(&self, update: &ShellOutputUpdate) -> usize {
        serde_json::to_string(update).map(|s| s.len()).unwrap_or(0)
    }

    fn publish(&self, update: ShellOutputUpdate) {
        if let Some(callback) = &self.on_update {
            callback(&update, &self.context);
        }
    }

    fn on_error(&self, _error: String) {}
}

/// 对应 `OutputCapture`。
pub struct OutputCapture {
    state: Arc<Mutex<OutputCaptureState>>,
    max_bytes: usize,
    max_lines: usize,
    retain: ShellOutputRetention,
    publisher: AdaptivePublisher<ShellOutputView, ShellOutputUpdate>,
}

impl OutputCapture {
    /// 对应 `constructor`。
    pub fn new(
        options: Option<&ShellOutputCaptureOptions>,
        context: Context,
        on_update: Option<ShellOutputUpdateCallback>,
    ) -> Self {
        let max_bytes = options
            .map(|o| o.limits.max_bytes)
            .unwrap_or(DEFAULT_MAX_BYTES);
        let max_lines = options
            .map(|o| o.limits.max_lines)
            .unwrap_or(DEFAULT_MAX_LINES);
        let retain = options
            .and_then(|o| o.limits.retain)
            .unwrap_or(ShellOutputRetention::Tail);
        assert!(max_bytes > 0, "Output maxBytes must be a positive number");
        assert!(max_lines > 0, "Output maxLines must be a positive integer");

        let state = Arc::new(Mutex::new(OutputCaptureState {
            buffer: String::new(),
            buffer_bytes: 0,
            total_bytes: 0,
            newlines: 0,
            ends_with_newline: true,
            current_line_bytes: 0,
            spill_path: None,
            disposed: false,
        }));

        let sink = Arc::new(CaptureSink {
            state: Arc::clone(&state),
            max_bytes,
            max_lines,
            retain,
            context,
            on_update,
        });

        let publisher = AdaptivePublisher::new(
            sink,
            OUTPUT_MIN_EMIT_INTERVAL_MS,
            OUTPUT_TARGET_BYTES_PER_SECOND,
        );

        Self {
            state,
            max_bytes,
            max_lines,
            retain,
            publisher,
        }
    }

    /// 对应 `get truncated`。
    pub fn truncated(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.total_bytes > self.max_bytes || total_lines(&state) > self.max_lines
    }

    /// 对应 `push(chunk)`。Rust 侧 stdout 已解码为 UTF-8，仅接受 `&str`。
    pub fn push(&self, chunk: &str) {
        {
            let mut state = self.state.lock().unwrap();
            if state.disposed {
                return;
            }
            append_text_inner(&mut state, chunk, self.max_bytes, self.retain);
        }
        self.publisher.mark_dirty();
    }

    /// 对应 `finish`。Rust 侧无 TextDecoder 流式缓冲，直接发布最终状态。
    pub fn finish(&self) {
        self.publisher.flush(true);
    }

    /// 对应 `setSpillPath`。
    pub fn set_spill_path(&self, path: &str) {
        {
            let mut state = self.state.lock().unwrap();
            if state.disposed || state.spill_path.as_deref() == Some(path) {
                return;
            }
            state.spill_path = Some(path.to_string());
        }
        self.publisher.mark_dirty();
        self.publisher.flush(true);
    }

    /// 对应 `snapshot`。
    pub fn snapshot(&self) -> ShellOutputView {
        let state = self.state.lock().unwrap();
        snapshot_inner(&state, self.max_bytes, self.max_lines, self.retain)
    }

    /// 对应 `flush`。
    pub fn flush(&self) {
        self.publisher.flush(true);
    }

    /// 对应 `dispose`。
    pub fn dispose(&self) {
        self.publisher.dispose();
        let mut state = self.state.lock().unwrap();
        state.disposed = true;
    }
}

fn total_lines(state: &OutputCaptureState) -> usize {
    state.newlines + if state.ends_with_newline || state.total_bytes == 0 { 0 } else { 1 }
}

fn append_text_inner(
    state: &mut OutputCaptureState,
    text: &str,
    max_bytes: usize,
    retain: ShellOutputRetention,
) {
    if text.is_empty() {
        return;
    }
    let text_bytes = text.len();
    state.total_bytes += text_bytes;
    state.newlines += count_newlines(text);
    state.ends_with_newline = text.ends_with('\n');
    let last_newline = text.rfind('\n');
    state.current_line_bytes = match last_newline {
        None => state.current_line_bytes + text_bytes,
        Some(index) => text[index + 1..].len(),
    };
    state.buffer.push_str(text);
    state.buffer_bytes += text_bytes;

    let guard = max_bytes * 2;
    if state.buffer_bytes > guard * 2 {
        state.buffer = match retain {
            ShellOutputRetention::Tail => trim_to_last_utf8_bytes(&state.buffer, guard),
            ShellOutputRetention::Head => trim_to_first_utf8_bytes(&state.buffer, guard),
        };
        state.buffer_bytes = state.buffer.len();
    }
}

/// 对应 `applyShellOutputUpdate`。
pub fn apply_shell_output_update(
    current: Option<&ShellOutputView>,
    update: &ShellOutputUpdate,
) -> ShellOutputView {
    match update {
        ShellOutputUpdate::Replace { output } => output.clone(),
        ShellOutputUpdate::Append { text, metadata } => ShellOutputView {
            text: format!("{}{text}", current.map(|c| c.text.as_str()).unwrap_or("")),
            truncation: metadata.truncation.clone(),
            spill_path: metadata.spill_path.clone(),
            last_line_bytes: metadata.last_line_bytes,
        },
        ShellOutputUpdate::Slide {
            drop,
            text,
            metadata,
        } => {
            let base = current.map(|c| c.text.as_str()).unwrap_or("");
            let kept = if *drop < base.len() { &base[*drop..] } else { "" };
            ShellOutputView {
                text: format!("{kept}{text}"),
                truncation: metadata.truncation.clone(),
                spill_path: metadata.spill_path.clone(),
                last_line_bytes: metadata.last_line_bytes,
            }
        }
        ShellOutputUpdate::Metadata { metadata } => ShellOutputView {
            text: current.map(|c| c.text.clone()).unwrap_or_default(),
            truncation: metadata.truncation.clone(),
            spill_path: metadata.spill_path.clone(),
            last_line_bytes: metadata.last_line_bytes,
        },
    }
}

fn snapshot_inner(
    state: &OutputCaptureState,
    max_bytes: usize,
    max_lines: usize,
    retain: ShellOutputRetention,
) -> ShellOutputView {
    let options = TruncationOptions {
        max_bytes: Some(max_bytes),
        max_lines: Some(max_lines),
    };
    let retained = match retain {
        ShellOutputRetention::Head => truncate_head(&state.buffer, options),
        ShellOutputRetention::Tail => truncate_tail(&state.buffer, options),
    };
    let total_lines = total_lines(state);
    let truncated = state.total_bytes > max_bytes || total_lines > max_lines;
    let truncation = ShellOutputTruncation {
        truncated,
        truncated_by: if truncated {
            Some(if total_lines > max_lines { "lines" } else { "bytes" }.to_string())
        } else {
            None
        },
        total_lines,
        total_bytes: state.total_bytes,
        output_lines: retained.output_lines,
        output_bytes: retained.output_bytes,
        last_line_partial: retained.last_line_partial,
        first_line_exceeds_limit: retained.first_line_exceeds_limit,
        max_lines,
        max_bytes,
    };
    ShellOutputView {
        text: sanitize_shell_output(&retained.content),
        truncation,
        spill_path: state.spill_path.clone(),
        last_line_bytes: if retained.last_line_partial {
            Some(state.current_line_bytes)
        } else {
            None
        },
    }
}

fn update_from(
    previous: Option<&ShellOutputView>,
    current: &ShellOutputView,
) -> Option<ShellOutputUpdate> {
    let Some(previous) = previous else {
        return Some(ShellOutputUpdate::Replace {
            output: current.clone(),
        });
    };
    let metadata = ShellOutputMetadata {
        truncation: current.truncation.clone(),
        spill_path: current.spill_path.clone(),
        last_line_bytes: current.last_line_bytes,
    };
    if current.text == previous.text {
        return Some(ShellOutputUpdate::Metadata { metadata });
    }
    if current.text.len() > previous.text.len() && current.text.starts_with(&previous.text) {
        return Some(ShellOutputUpdate::Append {
            text: current.text[previous.text.len()..].to_string(),
            metadata,
        });
    }
    let shared = suffix_prefix_overlap(
        &previous.text,
        &current.text,
        previous
            .text
            .len()
            .min(current.text.len())
            .min(current.truncation.max_bytes * 2),
    );
    if shared > 0 {
        return Some(ShellOutputUpdate::Slide {
            drop: previous.text.len() - shared,
            text: current.text[shared..].to_string(),
            metadata,
        });
    }
    Some(ShellOutputUpdate::Replace {
        output: current.clone(),
    })
}

fn suffix_prefix_overlap(before: &str, after: &str, scan: usize) -> usize {
    if before.is_empty() || after.is_empty() || scan == 0 {
        return 0;
    }
    let tail = if before.len() > scan {
        &before[before.len() - scan..]
    } else {
        before
    };
    for probe_length in [64usize.min(after.len()), 1] {
        let probe = &after[..probe_length];
        let mut candidates = 0;
        let mut index = 0;
        while let Some(found) = tail[index..].find(probe) {
            let absolute = index + found;
            candidates += 1;
            if candidates > 8 {
                break;
            }
            let overlap_length = tail.len() - absolute;
            if overlap_length <= after.len() && tail[absolute..] == after[..overlap_length] {
                return overlap_length;
            }
            index = absolute + 1;
        }
        if probe_length == 1 {
            break;
        }
    }
    0
}
