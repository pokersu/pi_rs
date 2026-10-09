//! 对应 `env/decode.ts`：逐块解码与整块解码一致的 UTF-8 解码器。

/// 对应 `rangeDecoder`：一个 `TextDecoder("utf-8", { ignoreBOM: true })`。
#[derive(Debug, Default)]
pub struct RangeDecoder {
    pending: Vec<u8>,
}

impl RangeDecoder {
    /// 构造。
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `decoder.decode(bytes, { stream: true })`：保留尾部不完整序列。
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        if self.pending.is_empty() && bytes.is_empty() {
            return String::new();
        }
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        let split = incomplete_suffix_start(&buffer);
        self.pending = buffer[split..].to_vec();
        String::from_utf8_lossy(&buffer[..split]).into_owned()
    }

    /// 对应 `decoder.decode()`：冲刷未完成的字符（成为 U+FFFD）。
    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

/// 对应 `startsWithBom`。
pub fn starts_with_bom(first_bytes: &[u8]) -> bool {
    first_bytes.first() == Some(&0xef)
        && first_bytes.get(1) == Some(&0xbb)
        && first_bytes.get(2) == Some(&0xbf)
}

/// 对应 `StreamDecoder`：逐块解码等价于整块解码，并在流开头剥离前导 U+FEFF。
#[derive(Debug, Default)]
pub struct StreamDecoder {
    decoder: RangeDecoder,
    started: bool,
}

impl StreamDecoder {
    /// 构造。
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `decode(bytes?)`：`None` 表示流结束。
    pub fn decode(&mut self, bytes: Option<&[u8]>) -> String {
        let text = match bytes {
            Some(bytes) => self.decoder.decode(bytes),
            None => self.decoder.flush(),
        };
        if self.started || text.is_empty() {
            return text;
        }
        self.started = true;
        // U+FEFF 只能编码为 EF BB BF，所以流开头的 U+FEFF 恰是 BOM。
        text.strip_prefix('\u{feff}')
            .map_or_else(|| text.clone(), str::to_string)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_bom_detects_utf8_bom() {
        assert!(starts_with_bom(&[0xef, 0xbb, 0xbf]));
        assert!(!starts_with_bom(&[0x61, 0x62, 0x63]));
        assert!(!starts_with_bom(&[]));
    }

    #[test]
    fn stream_decoder_strips_leading_bom() {
        let mut decoder = StreamDecoder::new();
        let mut text = decoder.decode(Some(&[0xef, 0xbb, 0xbf]));
        text.push_str(&decoder.decode(Some(b"hi")));
        text.push_str(&decoder.decode(None));
        assert_eq!(text, "hi");
    }
}
