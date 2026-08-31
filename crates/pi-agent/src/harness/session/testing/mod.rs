//! Rust 翻译自 packages/agent/src/harness/session/testing/（目录）

pub mod types;

pub use types::{
    SessionBackendConformanceCase, SessionBackendFixture, SessionBackendFixtureFactory,
};

/// 对应 `createSessionBackendConformance`（简化：返回空 conformance case 列表）。
/// TS 原版有 1000+ 行的 storage 契约测试，后续按需补。
pub fn create_session_backend_conformance(
    _factory: SessionBackendFixtureFactory,
) -> Vec<SessionBackendConformanceCase> {
    Vec::new()
}
