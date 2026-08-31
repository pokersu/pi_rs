//! Rust 翻译自 packages/agent/src/harness/session/testing/types.ts

use std::sync::Arc;

use crate::harness::session::types::SessionStorage;

/// 对应 `SessionBackendFixture`
pub trait SessionBackendFixture: Send + Sync {
    fn storage(&self) -> Arc<dyn SessionStorage>;
}

/// 对应 `SessionBackendFixtureFactory`
pub type SessionBackendFixtureFactory =
    Arc<dyn Fn() -> Box<dyn SessionBackendFixture> + Send + Sync>;

/// 对应 `SessionBackendConformanceCase`
#[allow(clippy::type_complexity)]
pub struct SessionBackendConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    pub run: Box<
        dyn FnOnce(
                Box<dyn SessionBackendFixture>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send,
    >,
}

impl SessionBackendConformanceCase {
    pub async fn run(self, fixture: Box<dyn SessionBackendFixture>) {
        (self.run)(fixture).await
    }
}
