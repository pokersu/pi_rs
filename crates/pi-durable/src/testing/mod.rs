//! 对应 `testing/`：Storage 与 ExecutionEnv 实现的参数化一致性测试。
//!
//! 上游把它作为库的一部分导出，供各存储后端包复用；Rust 侧同样放在库内，
//! `tests/` 里对每个后端各跑一遍 [`storage_conformance_cases`]。

pub mod env_conformance;
pub mod storage_benchmark;
pub mod storage_conformance;

pub use env_conformance::{EnvConformanceCase, env_conformance_cases};
pub use storage_benchmark::{
    STORAGE_MEMORY_SCALES, StorageBenchmarkDataset, StorageBenchmarkScale, StorageReadBenchmark,
    StorageReadBenchmarkRun, StorageWriteBenchmark, StorageWriteBenchmarkRun, TIMING_SCALE,
    background_context, seed_storage_benchmark, seed_storage_write_benchmark,
    storage_benchmark_primary_record_count, storage_read_benchmarks, storage_write_benchmarks,
};
pub use storage_conformance::{StorageConformanceCase, storage_conformance_cases};
