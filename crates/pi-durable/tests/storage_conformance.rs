//! `testing::storage_conformance` 对 memory / jsonl / sqlite 三个后端的参数化运行。

use std::sync::Arc;

use pi_durable::chord::context::{BACKGROUND_CONTEXT, Context};
use pi_durable::env::{FileSystem, InMemoryFileSystem};
use pi_durable::storage::{JsonlStorage, MemoryStorage, SqliteStorage};
use pi_durable::testing::storage_conformance_cases;

fn context() -> &'static dyn Context {
    BACKGROUND_CONTEXT.as_ref()
}

async fn run_memory() {
    for (name, run) in storage_conformance_cases() {
        let storage = Arc::new(MemoryStorage::new());
        println!("  [memory] {name}");
        run(storage.as_ref(), context()).await;
    }
}

async fn run_jsonl() {
    for (name, run) in storage_conformance_cases() {
        let fs = Arc::new(InMemoryFileSystem::new()) as Arc<dyn FileSystem>;
        let storage = JsonlStorage::open(fs, "/sessions", Default::default(), context())
            .await
            .expect("open jsonl");
        println!("  [jsonl] {name}");
        run(&storage, context()).await;
    }
}

async fn run_sqlite() {
    for (name, run) in storage_conformance_cases() {
        let storage = SqliteStorage::open_in_memory().expect("open sqlite");
        println!("  [sqlite] {name}");
        run(&storage, context()).await;
    }
}

#[tokio::test]
async fn memory_storage_conformance() {
    run_memory().await;
}

#[tokio::test]
async fn jsonl_storage_conformance() {
    run_jsonl().await;
}

#[tokio::test]
async fn sqlite_storage_conformance() {
    run_sqlite().await;
}
