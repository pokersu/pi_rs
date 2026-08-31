//! Rust 翻译自 packages/agent/src/stream-fn.ts

use std::sync::Mutex;

use crate::types::StreamFn;

static DEFAULT_STREAM_FN: Mutex<Option<StreamFn>> = Mutex::new(None);

/// 对应 `setDefaultStreamFn`
pub fn set_default_stream_fn(stream_fn: Option<StreamFn>) {
    *DEFAULT_STREAM_FN.lock().unwrap() = stream_fn;
}

/// 对应 `getDefaultStreamFn`
pub fn get_default_stream_fn() -> StreamFn {
    DEFAULT_STREAM_FN
		.lock()
		.unwrap()
		.clone()
		.expect("No default stream function configured. Pass streamFn explicitly or call setDefaultStreamFn().")
}
