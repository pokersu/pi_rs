//! 对应 `env/line-scan.ts`：从顺序字节流计算 [`LineScan`] 的增量扫描器。
//!
//! 解码尺寸使用与整块解码一致的流式解码器，因此环境可以在有界内存里扫描任意大小。

use crate::env::LineScan;
use crate::env::decode::{RangeDecoder, starts_with_bom};

const NEWLINE: u8 = 0x0a;

/// 对应 `LineScanner`。
pub struct LineScanner {
    start_line: usize,
    end_line: Option<usize>,
    position: usize,
    newlines: usize,
    line_start: usize,
    start: Option<usize>,
    end: Option<usize>,
    first_line_end: Option<usize>,
    last_line_start: Option<usize>,
    selected_bytes: usize,
    first_line_bytes: usize,
    selection: Option<RangeDecoder>,
    first_line: Option<RangeDecoder>,
    /// 前几个字节，直到确定是否是 BOM。
    head: Option<Vec<u8>>,
    bom: bool,
}

impl LineScanner {
    /// `start_line` 非负整数；`end_line` 缺省为到末尾，且必须 `> start_line`。
    pub fn new(start_line: usize, end_line: Option<usize>) -> Self {
        assert!(
            end_line.is_none_or(|end| end > start_line),
            "Invalid line range"
        );
        let mut scanner = Self {
            start_line,
            end_line,
            position: 0,
            newlines: 0,
            line_start: 0,
            start: None,
            end: None,
            first_line_end: None,
            last_line_start: None,
            selected_bytes: 0,
            first_line_bytes: 0,
            selection: None,
            first_line: None,
            head: Some(Vec::new()),
            bom: false,
        };
        if start_line == 0 {
            scanner.begin(0);
        }
        scanner
    }

    /// 按顺序喂入字节。
    pub fn push(&mut self, chunk: &[u8]) {
        if let Some(head) = self.head.as_mut() {
            let take = (3 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            if head.len() < 3 {
                return;
            }
            let head = self.head.take().expect("head");
            self.bom = starts_with_bom(&head);
            self.process(&head);
            self.process(&chunk[take..]);
            return;
        }
        self.process(chunk);
    }

    fn process(&mut self, chunk: &[u8]) {
        let base = self.position;
        let mut from = 0;
        let mut index = 0;
        while index < chunk.len() {
            if chunk[index] != NEWLINE {
                index += 1;
                continue;
            }
            // 换行结束 `newlines` 这一行；它只在所选行之间属于选择。
            self.feed(chunk, base, from, index);
            let line = self.newlines;
            let position = base + index;
            if line == self.start_line {
                self.end_first_line(position);
            }
            if self.end_line == Some(line + 1) {
                self.end_selection(position);
            }
            self.feed(chunk, base, index, index + 1);
            from = index + 1;
            self.newlines += 1;
            self.line_start = position + 1;
            if self.newlines == self.start_line {
                self.begin(self.line_start);
            }
            if self.end_line == Some(self.newlines + 1) {
                self.last_line_start = Some(self.line_start);
            }
            index += 1;
        }
        self.feed(chunk, base, from, chunk.len());
        self.position += chunk.len();
    }

    fn feed(&mut self, chunk: &[u8], base: usize, mut from: usize, to: usize) {
        // 整块解码会丢弃前导 BOM。
        if self.bom && base + from < 3 {
            from = to.min(3 - base);
        }
        if to <= from {
            return;
        }
        let bytes = &chunk[from..to];
        let mut added_selected = 0;
        if let Some(selection) = &mut self.selection {
            added_selected = selection.decode(bytes).len();
        }
        self.selected_bytes += added_selected;
        let mut added_first = 0;
        if let Some(first_line) = &mut self.first_line {
            added_first = first_line.decode(bytes).len();
        }
        self.first_line_bytes += added_first;
    }

    fn begin(&mut self, start: usize) {
        self.start = Some(start);
        if self.end_line == Some(self.start_line + 1) {
            self.last_line_start = Some(start);
        }
        self.selection = Some(RangeDecoder::new());
        self.first_line = Some(RangeDecoder::new());
    }

    fn end_first_line(&mut self, position: usize) {
        self.first_line_end = Some(position);
        if let Some(first_line) = &mut self.first_line {
            self.first_line_bytes += first_line.flush().len();
        }
        self.first_line = None;
    }

    fn end_selection(&mut self, position: usize) {
        self.end = Some(position);
        if let Some(selection) = &mut self.selection {
            self.selected_bytes += selection.flush().len();
        }
        self.selection = None;
    }

    /// 结束扫描并返回结果。
    pub fn finish(mut self) -> LineScan {
        if let Some(head) = self.head.take() {
            self.bom = starts_with_bom(&head);
            self.process(&head);
        }
        let size = self.position;
        let Some(start) = self.start else {
            return LineScan {
                newlines: self.newlines,
                start: size,
                end: size,
                first_line_end: size,
                last_line_start: size,
                selected_bytes: 0,
                first_line_bytes: 0,
            };
        };
        if self.first_line_end.is_none() {
            self.end_first_line(size);
        }
        if self.end.is_none() {
            self.end_selection(size);
        }
        LineScan {
            newlines: self.newlines,
            start,
            end: self.end.unwrap_or(size),
            first_line_end: self.first_line_end.unwrap_or(size),
            // 越过最后一行结束的选择以最后一行结束。
            last_line_start: self.last_line_start.unwrap_or(self.line_start),
            selected_bytes: self.selected_bytes,
            first_line_bytes: self.first_line_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_all_lines_of_a_simple_file() {
        let mut scanner = LineScanner::new(0, None);
        scanner.push(b"one\ntwo\nthree");
        let scan = scanner.finish();
        assert_eq!(scan.newlines, 2);
        assert_eq!(scan.start, 0);
        assert_eq!(scan.end, 13);
        assert_eq!(scan.selected_bytes, 13);
        assert_eq!(scan.first_line_bytes, 3);
    }

    #[test]
    fn selects_a_middle_line() {
        let mut scanner = LineScanner::new(1, Some(2));
        scanner.push(b"one\ntwo\nthree");
        let scan = scanner.finish();
        assert_eq!(scan.start, 4);
        assert_eq!(scan.end, 7);
        assert_eq!(scan.selected_bytes, 3);
        assert_eq!(scan.last_line_start, 4);
    }
}
