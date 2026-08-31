//! Rust 翻译自 packages/ai/src/utils/uuid.ts
//!
//! 生成时间有序的 UUIDv7。

use rand::RngCore;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct UuidV7State {
    last_timestamp: f64,
    sequence: u32,
}

static STATE: Mutex<UuidV7State> = Mutex::new(UuidV7State {
    last_timestamp: f64::NEG_INFINITY,
    sequence: 0,
});

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}

fn fill_random_bytes(bytes: &mut [u8; 16]) {
    rand::thread_rng().fill_bytes(bytes);
}

/// 对应 `uuidv7()`：生成时间有序的 UUIDv7。
pub fn uuidv7() -> String {
    let mut random = [0u8; 16];
    fill_random_bytes(&mut random);
    let timestamp = now_ms();

    let (last_timestamp, sequence) = {
        let mut state = STATE.lock().unwrap();
        if timestamp > state.last_timestamp {
            state.sequence = u32::from_be_bytes([random[6], random[7], random[8], random[9]]);
            state.last_timestamp = timestamp;
        } else {
            state.sequence = state.sequence.wrapping_add(1);
            if state.sequence == 0 {
                state.last_timestamp += 1.0;
            }
        }
        (state.last_timestamp, state.sequence)
    };

    let ts = last_timestamp as u64;
    let mut bytes = [0u8; 16];
    bytes[0] = (ts >> 40) as u8;
    bytes[1] = (ts >> 32) as u8;
    bytes[2] = (ts >> 24) as u8;
    bytes[3] = (ts >> 16) as u8;
    bytes[4] = (ts >> 8) as u8;
    bytes[5] = ts as u8;
    bytes[6] = 0x70 | ((sequence >> 28) & 0x0f) as u8;
    bytes[7] = (sequence >> 20) as u8;
    bytes[8] = 0x80 | ((sequence >> 14) & 0x3f) as u8;
    bytes[9] = (sequence >> 6) as u8;
    bytes[10] = (((sequence & 0x3f) << 2) as u8) | (random[10] & 0x03);
    bytes[11] = random[11];
    bytes[12] = random[12];
    bytes[13] = random[13];
    bytes[14] = random[14];
    bytes[15] = random[15];

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
