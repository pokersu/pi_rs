//! `harness::harness` 的端到端冒烟测试：打开 Harness、创建根会话并验证内置文档初始化。
//!
//! 覆盖：`open()` 的内置任务校验、`HarnessHooks::conversation_created` 的 `pi.*` 文档暂存、
//! 以及 `root()` / `close()` 的组装路径。

use std::sync::Arc;

use pi_ai::create_models;
use pi_durable::chord::context::{Context, EmptyContext};
use pi_durable::harness::agent::AGENT_DOC;
use pi_durable::harness::harness::open;
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::provider::PROVIDER_DOC;
use pi_durable::harness::registry::create_registry;
use pi_durable::harness::types::{Harness, HarnessOptions, RegistryReader};
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::session::{DocumentReader, Session};
use pi_durable::storage::MemoryStorage;
use pi_durable::types::Storage;

fn context() -> Arc<dyn Context> {
    Arc::new(EmptyContext::new("[test harness]"))
}

#[tokio::test]
async fn open_creates_root_conversation_with_builtin_documents() {
    let storage = Arc::new(MemoryStorage::new());
    let registry = create_registry();
    let models = Arc::new(create_models());
    let options = HarnessOptions {
        models,
        registry: registry as Arc<dyn RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: None,
        on_report: None,
    };
    let harness = open(Arc::clone(&storage) as Arc<dyn Storage>, options, context())
        .await
        .expect("open harness");

    let root = harness
        .root(context(), None, None)
        .await
        .expect("root conversation");
    assert_eq!(root.id().get(), 1);

    // 内置 `pi.*` 文档都在创建提交里暂存。
    for (token, expect) in [
        (&*LIVE_DOC, true),
        (&*INBOX_DOC, true),
        (&*USAGE_DOC, true),
        (&*PROVIDER_DOC, true),
        (&*AGENT_DOC, true),
    ] {
        let kind = token.definition().kind().to_string();
        let value = harness
            .snapshot(token, Some(1), None, context())
            .await
            .expect("snapshot");
        assert_eq!(value.is_some(), expect, "document {kind}");
    }

    // 再次调用 `root` 幂等返回同一个根会话。
    let again = harness
        .root(context(), None, None)
        .await
        .expect("root again");
    assert_eq!(again.id().get(), 1);

    harness.close(context()).await.expect("close");
}
