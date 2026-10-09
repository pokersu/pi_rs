//! `testing::storage_benchmark` 的读写基准对 memory / jsonl / sqlite 三个后端的参数化运行。

use std::sync::Arc;

use pi_durable::chord::context::{BACKGROUND_CONTEXT, Context};
use pi_durable::env::{FileSystem, InMemoryFileSystem};
use pi_durable::storage::{JsonlStorage, MemoryStorage, SqliteStorage};
use pi_durable::testing::{
    TIMING_SCALE, background_context, seed_storage_benchmark, seed_storage_write_benchmark,
    storage_read_benchmarks, storage_write_benchmarks,
};
use pi_durable::types::Storage;

async fn run_read(storage: &dyn Storage, label: &str) {
    let dataset = seed_storage_benchmark(storage, TIMING_SCALE, background_context()).await;
    for benchmark in storage_read_benchmarks() {
        let actual = (benchmark.run)(storage, &dataset, background_context()).await;
        let expected = (benchmark.expected)(&dataset);
        assert_eq!(
            actual, expected,
            "[{label}] read benchmark `{}`",
            benchmark.name
        );
    }
}

async fn run_write(storage: &dyn Storage, label: &str) {
    seed_storage_write_benchmark(storage, background_context()).await;
    for benchmark in storage_write_benchmarks() {
        let actual = (benchmark.run)(storage, background_context()).await;
        assert_eq!(
            actual, benchmark.expected,
            "[{label}] write benchmark `{}`",
            benchmark.name
        );
    }
}

fn context() -> &'static dyn Context {
    BACKGROUND_CONTEXT.as_ref()
}

#[tokio::test]
async fn memory_storage_benchmarks() {
    run_read(&MemoryStorage::new(), "memory").await;
    run_write(&MemoryStorage::new(), "memory").await;
}

#[tokio::test]
async fn jsonl_storage_benchmarks() {
    let fs = Arc::new(InMemoryFileSystem::new()) as Arc<dyn FileSystem>;
    let storage = JsonlStorage::open(fs, "/sessions", Default::default(), context())
        .await
        .expect("open jsonl");
    run_read(&storage, "jsonl").await;
    let fs = Arc::new(InMemoryFileSystem::new()) as Arc<dyn FileSystem>;
    let storage = JsonlStorage::open(fs, "/sessions", Default::default(), context())
        .await
        .expect("open jsonl");
    run_write(&storage, "jsonl").await;
}

#[tokio::test]
async fn sqlite_storage_benchmarks() {
    run_read(
        &SqliteStorage::open_in_memory().expect("open sqlite"),
        "sqlite",
    )
    .await;
    run_write(
        &SqliteStorage::open_in_memory().expect("open sqlite"),
        "sqlite",
    )
    .await;
}
