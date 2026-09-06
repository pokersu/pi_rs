//! Rust 翻译自 packages/ai/src/utils/uuid.ts
//!
//! 生成时间有序的 UUIDv7。支持 `uuidv7_with_timestamp` 为 follower id 保留
//! 指定时间戳（session fork 等场景需要保持时间顺序）。

use rand::RngCore;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 对应 `MAX_UUID_V7_TIMESTAMP`（48 位时间戳上限）。
const MAX_UUID_V7_TIMESTAMP: u64 = 0xffff_ffff_ffff;
/// 对应 `MAX_SEQUENCE`（41 位 sequence 上限）。
const MAX_SEQUENCE: u64 = (1u64 << 41) - 1;

struct UuidV7State {
    last_ordinary_timestamp: i64,
    sequence: Option<u64>,
}

static STATE: Mutex<UuidV7State> = Mutex::new(UuidV7State {
    last_ordinary_timestamp: -1,
    sequence: None,
});

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}

/// 对应 `uuidv7()`：生成时间有序的 UUIDv7。
pub fn uuidv7() -> String {
    uuidv7_inner(None)
}

/// 对应 `uuidv7(timestampMs)`：生成携带指定时间戳的 follower UUIDv7。
pub fn uuidv7_with_timestamp(timestamp_ms: u64) -> String {
    uuidv7_inner(Some(timestamp_ms))
}

fn uuidv7_inner(timestamp_ms: Option<u64>) -> String {
    let requested = timestamp_ms.unwrap_or_else(|| now_ms() as u64);
    assert!(
        requested <= MAX_UUID_V7_TIMESTAMP,
        "UUIDv7 timestamp must be an integer between 0 and {MAX_UUID_V7_TIMESTAMP}"
    );

    let (effective_timestamp, sequence) = {
        let mut state = STATE.lock().unwrap();
        let effective = match timestamp_ms {
            None => {
                let last = state.last_ordinary_timestamp;
                let e = if last >= 0 {
                    requested.max(last as u64)
                } else {
                    requested
                };
                state.last_ordinary_timestamp = e as i64;
                e
            }
            Some(ts) => ts,
        };
        let seq = match state.sequence {
            None => {
                let mut r = [0u8; 16];
                rand::thread_rng().fill_bytes(&mut r);
                let s = ((r[1] as u64) << 32)
                    | ((r[2] as u64) << 24)
                    | ((r[3] as u64) << 16)
                    | ((r[4] as u64) << 8)
                    | (r[5] as u64);
                state.sequence = Some(s);
                s
            }
            Some(s) => {
                if s == MAX_SEQUENCE {
                    panic!("UUIDv7 generator sequence exhausted");
                }
                let next = s + 1;
                state.sequence = Some(next);
                next
            }
        };
        (effective, seq)
    };

    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    for (i, byte) in bytes.iter_mut().take(6).enumerate() {
        *byte = ((effective_timestamp >> ((5 - i) * 8)) & 0xff) as u8;
    }
    bytes[6] = 0x70 | ((sequence >> 37) & 0x0f) as u8;
    bytes[7] = ((sequence >> 29) & 0xff) as u8;
    bytes[8] = 0x80 | ((sequence >> 23) & 0x3f) as u8;
    bytes[9] = ((sequence >> 15) & 0xff) as u8;
    bytes[10] = ((sequence >> 7) & 0xff) as u8;
    bytes[11] = (((sequence & 0x7f) << 1) as u8) | (bytes[11] & 0x01);

    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].join(""),
        hex[4..6].join(""),
        hex[6..8].join(""),
        hex[8..10].join(""),
        hex[10..16].join("")
    )
}
