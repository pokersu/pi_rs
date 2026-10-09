//! 对应 `src/storage/`：会话记录的持久化后端。
//!
//! 三个后端共享 [`Storage`](crate::types::Storage) 契约与 [`scan`] 的扫描起点/游标/分页：
//! [`memory`]（参考实现）、[`jsonl`]（两阶段写 + 重放）、[`sqlite`]（SQL 实现）。

pub mod jsonl;
pub mod memory;
pub mod scan;
pub mod sqlite;

pub use jsonl::{JsonlStorage, JsonlStorageOptions};
pub use memory::MemoryStorage;
pub use scan::{ScanError, ScanStart, next_cursor, scan_start};
pub use sqlite::SqliteStorage;
