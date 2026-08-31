//! Rust 翻译自 packages/telemetry/src/testing/index.ts

mod conformance;
mod types;

pub use conformance::create_telemetry_adapter_conformance;
pub use types::{
    TelemetryAdapterConformanceCase, TelemetryAdapterFixture, TelemetryAdapterFixtureFactory,
};
