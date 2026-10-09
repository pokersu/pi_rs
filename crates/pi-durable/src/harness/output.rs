//! 对应 `harness/output.ts`：工具输出的消毒、按行/字节裁剪与有界运行缓冲。
//!
//! 上游用 `TextEncoder` / `TextDecoder`（UTF-8 字节）做精确切片；Rust 的 `str` 本身就是 UTF-8，
//! 因此直接按字节切片，再用 `String::from_utf8_lossy` 复现解码器的替换语义。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - `setTimeout` / `Date.now` → `tokio::time::Instant` + `tokio::spawn`（无运行时则同步执行，
//!   与 `session::observation` 的既有做法一致）。
//! - `PromiseWithResolvers` → `oneshot`；[`Progress::stop`] 把未结清的等待交还调用方。
//! - 上游 `utf8ByteLength` → Rust 的 `str::len()`（已是字节长度）。

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use futures::channel::oneshot;
use futures::future::BoxFuture;
use tokio::time::Instant;

use crate::env::{ShellOutputSkip, ShellOutputStream, ShellOutputWindow};
use crate::harness::util::closed_error;
use crate::session::SessionError;

/// 对应 `OutputLimits`：一次工具输出的保留限制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    /// 保留的 UTF-8 字节上限。
    pub max_bytes: usize,
    /// 保留的行数上限。
    pub max_lines: usize,
    /// 保留输出的头部还是尾部。
    pub retain: Retain,
}

/// 对应 `retain: "head" | "tail"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retain {
    /// 保留开头。
    Head,
    /// 保留结尾。
    Tail,
}

impl OutputLimits {
    /// 由 `ShellOutputWindow` 构造尾部保留的限制（工具执行设施的常见入口）。
    pub fn from_window(window: &ShellOutputWindow) -> Self {
        Self {
            max_bytes: window.max_bytes,
            max_lines: window.max_lines,
            retain: Retain::Tail,
        }
    }
}

/// 对应 `BoundedOutput`：保留的输出与限制丢掉的部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    /// 保留并消毒后的文本。
    pub text: String,
    /// 丢弃的 UTF-8 字节数。
    pub dropped_bytes: usize,
    /// 丢弃的行数。
    pub dropped_lines: usize,
}

/// 对应 `OutputSlice`：输入在限制内的精确切片以及被留下的部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSlice {
    /// 切片文本。
    pub text: String,
    /// 切片的 UTF-8 字节数。
    pub bytes: usize,
    /// 丢弃的 UTF-8 字节数。
    pub dropped_bytes: usize,
    /// 丢弃的行数。
    pub dropped_lines: usize,
}

const NEWLINE: u8 = 0x0a;

/// 对应 `sanitizeOutput`：去掉破坏显示与转录的控制字符；制表符与换行保留。
pub fn sanitize_output(text: &str) -> String {
    if text.chars().all(is_valid_output_char) {
        return text.to_string();
    }
    text.chars().filter(|c| is_valid_output_char(*c)).collect()
}

/// 对应 `INVALID_OUTPUT = /[\x00-\x08\x0b-\x1f\ufff9-\ufffb]/`。
fn is_valid_output_char(c: char) -> bool {
    !matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{fff9}'..='\u{fffb}')
}

/// 对应 `boundOutput`：把 `text` 限制到整行——`head` 取开头若干行，`tail` 取结尾若干行。
///
/// 结果是精确切片（含行尾换行）；单行超过 `max_bytes` 时在字节上限处按字符边界截断。
pub fn bound_output(text: &str, limits: &OutputLimits) -> OutputSlice {
    let bytes = text.as_bytes();
    let (from, to) = match limits.retain {
        Retain::Head => head_range(bytes, limits),
        Retain::Tail => tail_range(bytes, limits),
    };
    let kept = &bytes[from..to];
    OutputSlice {
        text: if kept.len() == bytes.len() {
            text.to_string()
        } else {
            String::from_utf8_lossy(kept).into_owned()
        },
        bytes: kept.len(),
        dropped_bytes: bytes.len() - kept.len(),
        dropped_lines: line_count(bytes) - line_count(kept),
    }
}

/// 对应 `headRange`。
fn head_range(bytes: &[u8], limits: &OutputLimits) -> (usize, usize) {
    if limits.max_lines == 0 || limits.max_bytes == 0 {
        return (0, 0);
    }
    let mut end = bytes.len();
    let mut lines = 0usize;
    let mut index = index_of(bytes, NEWLINE, 0);
    while let Some(found) = index {
        lines += 1;
        if lines == limits.max_lines {
            end = found + 1;
            break;
        }
        index = index_of(bytes, NEWLINE, found + 1);
    }
    if end > limits.max_bytes {
        let newline = last_index_of_at(bytes, NEWLINE, limits.max_bytes - 1);
        end = match newline {
            Some(found) => found + 1,
            None => character_end(bytes, limits.max_bytes),
        };
    }
    (0, end)
}

/// 对应 `tailRange`。
fn tail_range(bytes: &[u8], limits: &OutputLimits) -> (usize, usize) {
    if limits.max_lines == 0 || limits.max_bytes == 0 {
        return (bytes.len(), bytes.len());
    }
    // 末尾换行结束最后一行，而不是开启新的一行。
    let last = if bytes.last() == Some(&NEWLINE) {
        bytes.len() as isize - 2
    } else {
        bytes.len() as isize - 1
    };
    let mut start = 0usize;
    let mut lines = 1usize;
    let mut index = if last < 0 {
        None
    } else {
        last_index_of_at(bytes, NEWLINE, last as usize)
    };
    while let Some(found) = index {
        if lines == limits.max_lines {
            start = found + 1;
            break;
        }
        lines += 1;
        index = if found == 0 {
            None
        } else {
            last_index_of_at(bytes, NEWLINE, found - 1)
        };
    }
    if bytes.len() - start > limits.max_bytes {
        let from = bytes.len() - limits.max_bytes;
        let newline = index_of(bytes, NEWLINE, from.saturating_sub(1));
        start = match newline {
            Some(found) if found + 1 < bytes.len() => found + 1,
            _ => character_start(bytes, from),
        };
    }
    (start, bytes.len())
}

/// 对应 `characterEnd`：`index` 之前（含）最后一个字符边界。
pub fn character_end(bytes: &[u8], index: usize) -> usize {
    let mut end = index;
    while end > 0 && (bytes.get(end).copied().unwrap_or(0) & 0xc0) == 0x80 {
        end -= 1;
    }
    end
}

/// 对应 `characterStart`：`index` 之后（含）第一个字符边界。
fn character_start(bytes: &[u8], index: usize) -> usize {
    let mut start = index;
    while start < bytes.len() && (bytes.get(start).copied().unwrap_or(0) & 0xc0) == 0x80 {
        start += 1;
    }
    start
}

/// 对应 `lineCount`。
fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut newlines = 0usize;
    let mut index = index_of(bytes, NEWLINE, 0);
    while let Some(found) = index {
        newlines += 1;
        index = index_of(bytes, NEWLINE, found + 1);
    }
    newlines + usize::from(bytes.last() != Some(&NEWLINE))
}

/// 对应 `lines(newlines, terminated)`。
fn lines(newlines: usize, terminated: bool) -> usize {
    newlines + usize::from(!terminated)
}

fn count_newlines(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == NEWLINE).count()
}

/// 对应 `Uint8Array.indexOf`。
fn index_of(bytes: &[u8], needle: u8, from: usize) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..]
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| offset + from)
}

/// 对应 `Uint8Array.lastIndexOf(needle, from)`（`from` 已确保非负）。
fn last_index_of_at(bytes: &[u8], needle: u8, from: usize) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let end = from.min(bytes.len() - 1);
    bytes[..=end].iter().rposition(|byte| *byte == needle)
}

/// 对应 `tailMargin`：`text` 中「字节数超过 `max_bytes` 或换行数超过 `max_lines`」的最短后缀，或全部。
///
/// 任何以此外缀结尾的文本，其后接任何内容的尾部窗口，都与「`text` 后接该内容」相同。
fn tail_margin(text: &str, limits: &OutputLimits) -> String {
    let bytes = text.as_bytes();
    let byte_start = if bytes.len() > limits.max_bytes {
        character_end(bytes, bytes.len() - limits.max_bytes - 1)
    } else {
        0
    };
    let mut line_start = 0usize;
    let mut newlines = 0usize;
    let mut index = bytes.iter().rposition(|byte| *byte == NEWLINE);
    while let Some(found) = index {
        newlines += 1;
        if newlines > limits.max_lines {
            line_start = found;
            break;
        }
        if found == 0 {
            break;
        }
        index = last_index_of_at(bytes, NEWLINE, found - 1);
    }
    let start = byte_start.max(line_start);
    if start == 0 {
        text.to_string()
    } else {
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
}

/// 对应 `string | Uint8Array`：一次推入的输出块。
#[derive(Debug, Clone, Copy)]
pub enum OutputChunk<'a> {
    /// 已解码的文本。
    Text(&'a str),
    /// 原始字节。
    Bytes(&'a [u8]),
}

/// UTF-8 增量解码器（对应 `TextDecoder("utf-8", { ignoreBOM: true })`）。
///
/// `ignore_bom` 语义由调用方显式处理（`OutputBuffer` 只剥离整个输出最开头的 BOM）。
#[derive(Debug, Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    /// 对应 `decoder.decode()`：冲刷未完成的字符（成为 U+FFFD）。
    fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }

    /// 对应 `decoder.decode(bytes, { stream: true })`：保留尾部不完整序列。
    fn decode(&mut self, bytes: &[u8]) -> String {
        if self.pending.is_empty() && bytes.is_empty() {
            return String::new();
        }
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        let split = incomplete_suffix_start(&buffer);
        self.pending = buffer[split..].to_vec();
        String::from_utf8_lossy(&buffer[..split]).into_owned()
    }
}

/// 返回尾部不完整 UTF-8 序列的起始下标；没有不完整序列时返回 `bytes.len()`。
fn incomplete_suffix_start(bytes: &[u8]) -> usize {
    let len = bytes.len();
    let mut index = len;
    let mut lookback = 0usize;
    while index > 0 && lookback < 3 {
        index -= 1;
        let byte = bytes[index];
        if byte & 0xc0 != 0x80 {
            let needed = match byte {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            return if needed > len - index { index } else { len };
        }
        lookback += 1;
    }
    len
}

/// 对应 `OutputBuffer`：一次工具调用的有界运行输出。
///
/// 接受一个块的代价与该块成正比：头部保留在窗口装满后停止存储，尾部保留在快照时丢弃窗口不再需要的文本。
/// 整个流的计数始终保留，因此丢弃总量始终精确。
#[derive(Debug)]
pub struct OutputBuffer {
    limits: OutputLimits,
    decoder: Utf8Stream,
    started: bool,
    /// 已存块：`head` 时为流的开头，`tail` 时为仍包含下一个窗口的后缀。
    chunks: Vec<StoredChunk>,
    stored_bytes: usize,
    stored_newlines: usize,
    full: bool,
    total_bytes: usize,
    total_newlines: usize,
    ends_with_newline: bool,
}

#[derive(Debug, Clone)]
struct StoredChunk {
    text: String,
    bytes: usize,
    newlines: usize,
}

impl OutputBuffer {
    /// 对应 `new OutputBuffer(limits)`。
    pub fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            decoder: Utf8Stream::default(),
            started: false,
            chunks: Vec::new(),
            stored_bytes: 0,
            stored_newlines: 0,
            full: false,
            total_bytes: 0,
            total_newlines: 0,
            ends_with_newline: true,
        }
    }

    /// 当前持有的字节数（受限制约束，另加一个块）。
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }

    /// 对应 `push(chunk, skipped?)`：接受一个块，返回是否接受了任何内容。
    ///
    /// `skipped` 是紧邻该块之前被省略的输出（对应 `ShellOutputInfo.skipped`，
    /// 必须比尾部窗口多出至少一字节或一行）；只有尾部保留接受它。
    pub fn push(&mut self, chunk: OutputChunk<'_>, skipped: Option<ShellOutputSkip>) -> bool {
        // 更早的字节块中不完整字符的字节先来。
        let pending = match chunk {
            OutputChunk::Text(_) => self.decoder.flush(),
            OutputChunk::Bytes(_) if skipped.is_some() => self.decoder.flush(),
            OutputChunk::Bytes(_) => String::new(),
        };
        let mut text = match chunk {
            OutputChunk::Text(text) => text.to_string(),
            OutputChunk::Bytes(bytes) => self.decoder.decode(bytes),
        };
        let first = !self.started && pending.is_empty() && skipped.is_none();
        if !pending.is_empty() || !text.is_empty() || skipped.is_some() {
            self.started = true;
        }
        if first && matches!(chunk, OutputChunk::Bytes(_)) && text.starts_with('\u{feff}') {
            text = text['\u{feff}'.len_utf8()..].to_string();
        }
        let Some(skipped) = skipped else {
            return self.accept(&(pending + &text));
        };
        if self.limits.retain != Retain::Tail {
            panic!("Skipped output requires tail retention");
        }
        self.accept(&pending);
        self.skip(skipped);
        self.accept(&text);
        true
    }

    /// 对应 `#skip`：计入被省略的输出。
    fn skip(&mut self, skipped: ShellOutputSkip) {
        if skipped.bytes == 0 {
            return;
        }
        self.total_bytes += skipped.bytes;
        self.total_newlines += skipped.newlines;
        self.ends_with_newline = skipped.ends_with_newline;
        self.chunks.clear();
        self.stored_bytes = 0;
        self.stored_newlines = 0;
    }

    /// 对应 `end()`：把不完整的尾部字符冲刷为替换字符；在流结束时调用。
    pub fn end(&mut self) {
        let text = self.decoder.flush();
        self.accept(&text);
    }

    /// 对应 `#accept`。
    fn accept(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let bytes = text.len();
        let newlines = count_newlines(text);
        self.total_bytes += bytes;
        self.total_newlines += newlines;
        self.ends_with_newline = text.ends_with('\n');
        if self.full {
            return true;
        }
        self.chunks.push(StoredChunk {
            text: text.to_string(),
            bytes,
            newlines,
        });
        self.stored_bytes += bytes;
        self.stored_newlines += newlines;
        if self.limits.retain == Retain::Head {
            // 窗口装满之后的内容永远不再需要。
            self.full = self.stored_bytes > self.limits.max_bytes
                || self.stored_newlines >= self.limits.max_lines;
            return true;
        }
        // 只要剩余部分仍多于一个窗口（字节多于 `maxBytes`，或换行多于 `maxLines`，各多一），
        // 就丢弃开头的块——「各多一」让窗口的行首仍可被找到。每个块只丢一次。
        while self.chunks.len() > 1 {
            let first = &self.chunks[0];
            let bytes_after = self.stored_bytes - first.bytes;
            let newlines_after = self.stored_newlines - first.newlines;
            if bytes_after <= self.limits.max_bytes + 1
                && newlines_after <= self.limits.max_lines + 1
            {
                break;
            }
            self.chunks.remove(0);
            self.stored_bytes = bytes_after;
            self.stored_newlines = newlines_after;
        }
        true
    }

    /// 对应 `snapshot()`：保留并消毒后的输出，以及限制从整个流中丢掉的部分。
    pub fn snapshot(&mut self) -> BoundedOutput {
        let stored = if self.chunks.len() == 1 {
            self.chunks[0].text.clone()
        } else {
            self.chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>()
        };
        let kept = bound_output(&stored, &self.limits);
        let stored_lines = lines(
            self.stored_newlines,
            stored.is_empty() || stored.ends_with('\n'),
        );
        let kept_lines = stored_lines - kept.dropped_lines;
        // 尾部窗口不会回溯到更早的内容，但要找下一个窗口的首行仍需要它之前的内容：
        // 保留「比窗口多一字节或一行」的最短后缀，与 `#accept` 一致。
        if self.limits.retain == Retain::Tail || self.chunks.len() > 1 {
            let text = if self.limits.retain == Retain::Tail {
                tail_margin(&stored, &self.limits)
            } else {
                stored
            };
            let bytes = if self.limits.retain == Retain::Tail {
                text.len()
            } else {
                self.stored_bytes
            };
            self.stored_newlines = if text.is_empty() {
                self.chunks = Vec::new();
                0
            } else {
                let newlines = count_newlines(&text);
                self.chunks = vec![StoredChunk {
                    text,
                    bytes,
                    newlines,
                }];
                newlines
            };
            self.stored_bytes = bytes;
        }
        BoundedOutput {
            text: sanitize_output(&kept.text),
            dropped_bytes: self.total_bytes - kept.bytes,
            dropped_lines: lines(self.total_newlines, self.ends_with_newline) - kept_lines,
        }
    }
}

/// 便于阅读：一次输出块的来源流（对应 `ShellOutputInfo.stream`，仅作文档用途）。
#[allow(dead_code)]
fn stream_name(stream: ShellOutputStream) -> &'static str {
    match stream {
        ShellOutputStream::Stdout => "stdout",
        ShellOutputStream::Stderr => "stderr",
    }
}

// ─── 进度提交节流 ────────────────────────────────────────────────────────────

/// 对应 `PROGRESS_BYTES_PER_SECOND`：每次进度提交也会按写入量买下一段暂停。
pub const PROGRESS_BYTES_PER_SECOND: u64 = 100 * 1024;

/// 对应 `PromiseWithResolvers<void>` 的结清句柄：由调用方在 `stop()` 之后结清。
pub struct ProgressWaiter {
    sender: Option<oneshot::Sender<Result<(), SessionError>>>,
}

impl ProgressWaiter {
    /// 结清为成功。
    pub fn resolve(mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Ok(()));
        }
    }

    /// 以错误结清（对应上游调用方的 `reject`）。
    pub fn reject(mut self, error: SessionError) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(error));
        }
    }
}

/// 对应 `Progress` 的写入回调：返回本次提交写入的字节数。
pub type ProgressWrite =
    Arc<dyn Fn() -> BoxFuture<'static, Result<usize, SessionError>> + Send + Sync>;

/// 对应 `Progress` 的错误回调。
pub type ProgressErrorReporter = Arc<dyn Fn(SessionError) + Send + Sync>;

struct ProgressState {
    waiters: Vec<oneshot::Sender<Result<(), SessionError>>>,
    /// 每安排一次定时器就自增，使旧的定时唤醒失效。
    generation: u64,
    in_flight: bool,
    next_at: Instant,
    dirty: bool,
    stopped: bool,
}

/// 对应 `Progress`：自适应的进度提交节流。
///
/// 空闲期后的第一次改动立即提交；此后每次提交把下一次至少推迟 `min_interval_ms`，并按写入量以
/// 100 KiB/s 再推迟一段。至多一次提交在途；期间的改动合并进下一次。
///
/// # 与上游的差异
///
/// 上游的定时器可直接从内部回调访问自身；Rust 侧 [`Progress::new`] 返回 [`Arc`]，
/// 定时器与提交任务通过内部弱引用驱动。
pub struct Progress {
    state: Mutex<ProgressState>,
    write: ProgressWrite,
    on_error: ProgressErrorReporter,
    min_interval_ms: u64,
    self_ref: OnceLock<Weak<Progress>>,
}

impl Progress {
    /// 对应 `new Progress(write, onError, minIntervalMs)`。
    pub fn new(
        write: ProgressWrite,
        on_error: ProgressErrorReporter,
        min_interval_ms: u64,
    ) -> Arc<Self> {
        let progress = Arc::new(Self {
            state: Mutex::new(ProgressState {
                waiters: Vec::new(),
                generation: 0,
                in_flight: false,
                next_at: Instant::now(),
                dirty: false,
                stopped: false,
            }),
            write,
            on_error,
            min_interval_ms,
            self_ref: OnceLock::new(),
        });
        let _ = progress.self_ref.set(Arc::downgrade(&progress));
        progress
    }

    /// 对应 `mark()`：安排一次提交。
    pub fn mark(&self) {
        {
            let mut state = self.state.lock().expect("progress");
            state.dirty = true;
        }
        self.schedule();
    }

    /// 对应 `markAndWait()`：安排一次提交；等待的 future 由包含本次改动的那次提交结清。
    pub fn mark_and_wait(&self) -> BoxFuture<'static, Result<(), SessionError>> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("progress");
            state.waiters.push(sender);
            state.dirty = true;
        }
        self.schedule();
        Box::pin(async move { receiver.await.unwrap_or_else(|_| Err(closed_error())) })
    }

    /// 对应 `stop()`：停止提交并等待在途提交；返回由最后一次提交结清的等待。
    pub fn stop(self: &Arc<Self>) -> BoxFuture<'static, Vec<ProgressWaiter>> {
        let waiters: Vec<ProgressWaiter> = {
            let mut state = self.state.lock().expect("progress");
            state.stopped = true;
            state.generation += 1;
            state
                .waiters
                .drain(..)
                .map(|sender| ProgressWaiter {
                    sender: Some(sender),
                })
                .collect()
        };
        // 上游 `await this.#inFlight`：在途提交结束后才交还等待者。
        let progress = Arc::clone(self);
        Box::pin(async move {
            while progress.state.lock().expect("progress").in_flight {
                tokio::task::yield_now().await;
            }
            waiters
        })
    }

    /// 对应 `#schedule()`。
    fn schedule(&self) {
        let (generation, wait) = {
            let mut state = self.state.lock().expect("progress");
            if state.stopped || state.in_flight {
                return;
            }
            let wait = state.next_at.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                drop(state);
                self.flush();
                return;
            }
            state.generation += 1;
            (state.generation, wait)
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let weak = self.self_ref.get().cloned();
                handle.spawn(async move {
                    tokio::time::sleep(wait).await;
                    if let Some(progress) = weak.and_then(|weak| weak.upgrade()) {
                        progress.wake(generation);
                    }
                });
            }
            // 无运行时：退化为立即提交（上游的定时器总会触发）。
            Err(_) => self.flush(),
        }
    }

    /// 定时唤醒：仅当仍是最新一代时提交。
    fn wake(&self, generation: u64) {
        {
            let state = self.state.lock().expect("progress");
            if state.stopped || state.generation != generation {
                return;
            }
        }
        self.flush();
    }

    /// 对应 `#flush()`。
    fn flush(&self) {
        let waiters = {
            let mut state = self.state.lock().expect("progress");
            if state.stopped || !state.dirty {
                return;
            }
            state.dirty = false;
            state.in_flight = true;
            std::mem::take(&mut state.waiters)
        };
        let started = Instant::now();
        let write = Arc::clone(&self.write);
        let min_interval_ms = self.min_interval_ms;
        let weak = self.self_ref.get().cloned();
        let run: BoxFuture<'static, ()> = Box::pin(async move {
            let outcome = write().await;
            if let Some(progress) = weak.and_then(|weak| weak.upgrade()) {
                progress.finish(started, outcome, waiters, min_interval_ms);
            }
        });
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(run);
            }
            // 无运行时：同步驱动一次提交。
            Err(_) => futures::executor::block_on(run),
        }
    }

    fn finish(
        &self,
        started: Instant,
        outcome: Result<usize, SessionError>,
        waiters: Vec<oneshot::Sender<Result<(), SessionError>>>,
        min_interval_ms: u64,
    ) {
        let dirty = {
            let mut state = self.state.lock().expect("progress");
            state.in_flight = false;
            match &outcome {
                Ok(bytes) => {
                    let charged = (*bytes as u64).saturating_mul(1000) / PROGRESS_BYTES_PER_SECOND;
                    state.next_at = started + Duration::from_millis(min_interval_ms.max(charged));
                }
                Err(_) => {
                    state.next_at = started + Duration::from_millis(min_interval_ms);
                }
            }
            state.dirty
        };
        match outcome {
            Ok(_) => {
                for waiter in waiters {
                    let _ = waiter.send(Ok(()));
                }
            }
            Err(error) => {
                for waiter in waiters {
                    let _ = waiter.send(Err(error.clone()));
                }
                (self.on_error)(error);
            }
        }
        if dirty {
            self.schedule();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// 一个记录调用次数与返回字节数的写入桩。
    fn write_stub(calls: Arc<AtomicU64>, bytes: u64) -> ProgressWrite {
        Arc::new(move || {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(bytes as usize)
            })
        })
    }

    async fn wait_until(predicate: impl Fn() -> bool) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("条件在超时前未满足");
    }

    #[tokio::test]
    async fn progress_commits_at_once_after_an_idle_period() {
        let calls = Arc::new(AtomicU64::new(0));
        let progress = Progress::new(write_stub(Arc::clone(&calls), 0), Arc::new(|_| {}), 60_000);
        progress.mark();
        wait_until(|| calls.load(Ordering::Relaxed) == 1).await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "空闲后的第一次提交立即发生"
        );
    }

    #[tokio::test]
    async fn progress_delays_the_next_commit_by_the_minimum_interval() {
        let calls = Arc::new(AtomicU64::new(0));
        let progress = Progress::new(write_stub(Arc::clone(&calls), 0), Arc::new(|_| {}), 40);
        progress.mark();
        wait_until(|| calls.load(Ordering::Relaxed) == 1).await;

        // 紧接着再一次：应被推迟到最小间隔之后。
        progress.mark();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "最小间隔内的第二次提交被推迟"
        );
        wait_until(|| calls.load(Ordering::Relaxed) == 2).await;
    }

    #[tokio::test]
    async fn progress_charges_a_pause_proportional_to_the_written_size() {
        let calls = Arc::new(AtomicU64::new(0));
        // 写入 100 KiB 需要整整一秒的计费暂停（100 KiB/s）。
        let progress = Progress::new(
            write_stub(Arc::clone(&calls), PROGRESS_BYTES_PER_SECOND),
            Arc::new(|_| {}),
            1,
        );
        progress.mark();
        wait_until(|| calls.load(Ordering::Relaxed) == 1).await;

        progress.mark();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "大提交按 100 KiB/s 计费，不会在一瞬间再次提交"
        );
    }

    #[tokio::test]
    async fn progress_mark_and_wait_settles_with_its_commit() {
        let calls = Arc::new(AtomicU64::new(0));
        let progress = Progress::new(write_stub(Arc::clone(&calls), 0), Arc::new(|_| {}), 10);
        progress.mark_and_wait().await.expect("settled");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn progress_reports_a_write_failure() {
        let errors = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&errors);
        let progress = Progress::new(
            Arc::new(|| Box::pin(async { Err(SessionError::Message("boom".to_string())) })),
            Arc::new(move |_| {
                seen.fetch_add(1, Ordering::Relaxed);
            }),
            10,
        );
        let outcome = progress.mark_and_wait().await;
        assert!(outcome.is_err(), "失败的提交以错误结清等待者");
        wait_until(|| errors.load(Ordering::Relaxed) == 1).await;
    }

    #[tokio::test]
    async fn progress_stop_hands_back_unsettled_waiters() {
        let calls = Arc::new(AtomicU64::new(0));
        let progress = Progress::new(write_stub(Arc::clone(&calls), 0), Arc::new(|_| {}), 10);
        progress.mark();
        wait_until(|| calls.load(Ordering::Relaxed) == 1).await;

        // 一次被推迟的改动：它的等待者尚未结清。
        let pending = progress.mark_and_wait();
        let waiters = progress.stop().await;
        assert_eq!(waiters.len(), 1, "未结清的等待交还调用方");
        for waiter in waiters {
            waiter.resolve();
        }
        pending.await.expect("由调用方结清");

        // 停止之后不再提交。
        let before = calls.load(Ordering::Relaxed);
        progress.mark();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(calls.load(Ordering::Relaxed), before, "停止后不再提交");
    }

    fn limits(max_bytes: usize, max_lines: usize, retain: Retain) -> OutputLimits {
        OutputLimits {
            max_bytes,
            max_lines,
            retain,
        }
    }

    #[test]
    fn sanitize_keeps_tabs_newlines_and_drops_control_characters() {
        assert_eq!(sanitize_output("a\tb\nc"), "a\tb\nc");
        assert_eq!(sanitize_output("a\u{0}b\u{1f}c"), "abc");
        assert_eq!(sanitize_output("x\u{fff9}y"), "xy");
        assert_eq!(sanitize_output("ok"), "ok");
    }

    #[test]
    fn head_retention_keeps_whole_lines() {
        let slice = bound_output("one\ntwo\nthree\n", &limits(1024, 2, Retain::Head));
        assert_eq!(slice.text, "one\ntwo\n");
        assert_eq!(slice.dropped_lines, 1);
        assert_eq!(slice.bytes, 8);
    }

    #[test]
    fn tail_retention_keeps_whole_lines() {
        let slice = bound_output("one\ntwo\nthree\n", &limits(1024, 2, Retain::Tail));
        assert_eq!(slice.text, "two\nthree\n");
        assert_eq!(slice.dropped_lines, 1);
    }

    #[test]
    fn single_long_line_is_cut_on_a_character_boundary() {
        // 「é」是 2 字节；在 3 字节窗口内只能放下一个完整字符。
        let slice = bound_output("ééé", &limits(3, 10, Retain::Head));
        assert_eq!(slice.text, "é");
        assert_eq!(slice.bytes, 2);
        assert_eq!(slice.dropped_bytes, 4);
    }

    #[test]
    fn zero_limits_keep_nothing() {
        let head = bound_output("abc", &limits(0, 5, Retain::Head));
        assert_eq!(head.text, "");
        let tail = bound_output("abc", &limits(5, 0, Retain::Tail));
        assert_eq!(tail.text, "");
    }

    #[test]
    fn character_helpers_snap_to_boundaries() {
        let bytes = "é".as_bytes();
        assert_eq!(character_end(bytes, 1), 0);
        assert_eq!(character_start(bytes, 1), 2);
        assert_eq!(character_end(bytes, 2), 2);
        assert_eq!(character_start(bytes, 0), 0);
    }

    #[test]
    fn buffer_tail_retains_the_window_and_counts_dropped_lines() {
        let mut buffer = OutputBuffer::new(limits(1024, 2, Retain::Tail));
        buffer.push(OutputChunk::Text("one\n"), None);
        buffer.push(OutputChunk::Text("two\n"), None);
        buffer.push(OutputChunk::Text("three\n"), None);
        let out = buffer.snapshot();
        assert_eq!(out.text, "two\nthree\n");
        assert_eq!(out.dropped_lines, 1);
        assert_eq!(out.dropped_bytes, 4);
    }

    #[test]
    fn buffer_head_stops_storing_once_full_but_counts_everything() {
        let mut buffer = OutputBuffer::new(limits(8, 2, Retain::Head));
        buffer.push(OutputChunk::Text("one\n"), None);
        buffer.push(OutputChunk::Text("two\n"), None);
        buffer.push(OutputChunk::Text("three\n"), None);
        let out = buffer.snapshot();
        assert_eq!(out.text, "one\ntwo\n");
        assert_eq!(out.dropped_lines, 1);
    }

    #[test]
    fn buffer_decodes_split_characters_across_byte_chunks() {
        let mut buffer = OutputBuffer::new(limits(64, 10, Retain::Tail));
        let text = "aé";
        let bytes = text.as_bytes();
        for byte in bytes {
            buffer.push(OutputChunk::Bytes(&[*byte]), None);
        }
        buffer.end();
        assert_eq!(buffer.snapshot().text, "aé");
    }

    #[test]
    fn buffer_drops_only_a_leading_byte_order_mark() {
        let mut buffer = OutputBuffer::new(limits(64, 10, Retain::Tail));
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice("ok".as_bytes());
        buffer.push(OutputChunk::Bytes(&bytes), None);
        assert_eq!(buffer.snapshot().text, "ok");

        let mut later = OutputBuffer::new(limits(64, 10, Retain::Tail));
        later.push(OutputChunk::Bytes(&bytes), None);
        later.push(OutputChunk::Bytes(&[0xef, 0xbb, 0xbf]), None);
        assert_eq!(later.snapshot().text, "ok\u{feff}");
    }

    #[test]
    fn buffer_flushes_an_incomplete_trailing_character() {
        let mut buffer = OutputBuffer::new(limits(64, 10, Retain::Tail));
        buffer.push(OutputChunk::Bytes(&[0xe2, 0x82]), None);
        buffer.end();
        assert_eq!(buffer.snapshot().text, "\u{fffd}");
    }

    #[test]
    fn buffer_accepts_a_tail_skip() {
        let mut buffer = OutputBuffer::new(limits(64, 10, Retain::Tail));
        buffer.push(OutputChunk::Text("head\n"), None);
        let accepted = buffer.push(
            OutputChunk::Text("tail\n"),
            Some(ShellOutputSkip {
                bytes: 5,
                newlines: 1,
                ends_with_newline: true,
            }),
        );
        assert!(accepted);
        let out = buffer.snapshot();
        assert_eq!(out.text, "tail\n");
        // 整个流 = head(5) + skipped(5) + tail(5)，窗口只留下 tail。
        assert_eq!(out.dropped_bytes, 10);
        assert_eq!(out.dropped_lines, 2);
    }

    #[test]
    #[should_panic(expected = "Skipped output requires tail retention")]
    fn buffer_rejects_a_skip_with_head_retention() {
        let mut buffer = OutputBuffer::new(limits(64, 10, Retain::Head));
        buffer.push(
            OutputChunk::Text("tail\n"),
            Some(ShellOutputSkip {
                bytes: 5,
                newlines: 1,
                ends_with_newline: true,
            }),
        );
    }

    #[test]
    fn buffer_sanitizes_snapshots() {
        let mut buffer = OutputBuffer::new(limits(64, 10, Retain::Tail));
        buffer.push(OutputChunk::Text("a\u{0}b\n"), None);
        assert_eq!(buffer.snapshot().text, "ab\n");
    }
}
