//! 对应 `src/storage/jsonl/`：JSONL 文件后端。
//!
//! 已实现编解码层（[`codec`]）与存储层（[`storage`]）。暂未实现 sidecar 回收（空间优化）。

pub mod codec;
pub mod storage;

pub use storage::{JsonlStorage, JsonlStorageOptions};

pub use codec::{
    EncodeError, EncodedCommit, FORMAT_VERSION, MAIN_FILE, MainMarker, MainOperation, MarkerKind,
    RECLAIM_SUFFIX, RecordKind, SidecarKind, SidecarPayload, SidecarRecord, StoredTask,
    encode_commit, is_current_only, is_reclaim_file_name, is_sidecar_file_name, sidecar_file_name,
    sidecar_key,
};
