//! 对应上游 `@earendil-works/chord` 中被 `pi-durable` 使用的子集。
//!
//! chord 是一个 9,008 行的应用组合运行时；`pi-durable` 只用到其中三块：
//!
//! | 上游 | 本模块 | 说明 |
//! |---|---|---|
//! | `chord/types.ts` 的 `Context` / `JsonValue` | [`context`] / [`json`] | 只需 `abortSignal`（durable 未使用 `ContextKey::value`） |
//! | `chord/context/index.ts` | [`context`] | `BACKGROUND_CONTEXT` / `withAbortSignal` / `withoutAbortSignal` / `awaitWithContext` |
//! | `chord/json.ts` | [`json`] | `copyJson`（Rust 的 `serde_json::Value` 天然是严格 JSON） |
//! | `chord/delta/index.ts` + `apply-immutable-trusted.ts` | [`delta`] | `Op` / `Path` / `apply` / `applyImmutable` / `applyImmutableBatches` / `encoder` / `decoder` |
//! | `chord/delta/tracker.ts` | [`tracker`] | `track` / `Tracker` / `Change` / `Prepared` |
//! | `chord/services/state.ts` + `state-internals.ts` | [`state`] | `ReplicatedState` / `MutableReplicatedState` / `ReplicatedStateReplica` |
//! | `chord/services/state-codec.ts` | [`state_codec`] | `ServiceStateEncoder` / `ServiceStateDecoder` |
//!
//! 未移植：`delta/{diff,draft,revision-validator}.ts`（`diff` 与 `Draft<T>` 类型级映射）、
//! `services/wire.ts` 的 `parse*`/`assert*` 协议校验、`facets/`、`node/`。

pub mod context;
pub mod delta;
pub mod json;
pub mod state;
pub mod state_codec;
pub mod tracker;

pub use context::{
    BACKGROUND_CONTEXT, Context, EmptyContext, TODO_CONTEXT, await_with_context, with_abort_signal,
    without_abort_signal,
};
pub use delta::{
    Decoder, DeltaError, Encoder, Op, Path, PathSegment, WireOp, apply, apply_immutable,
    apply_immutable_batches, decoder, encoder, overlap,
};
pub use json::{JsonValue, copy_json};
pub use state::{
    AttachedReplicatedState, DeliveryKind, MutableReplicatedState, MutableReplicatedStateImpl,
    ReplicatedState, ReplicatedStateDelivery, ReplicatedStateInternals, ReplicatedStatePublisher,
    ReplicatedStateReplica, ReplicatedStateSource, ReplicatedStateSourceAttachment,
    ReplicatedStateSourceFrame, StateErrorReporter, StateListener, Subscription,
    attach_replicated_state_source, get_replicated_state_internals,
    register_replicated_state_internals, replicated_state, service_delivery_context,
};
pub use state_codec::{
    ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshot, ServiceMode,
    ServiceProviderUpdate, ServiceStateDecoder, ServiceStateEncoder, ServiceSubscriptionSnapshot,
    WireServiceInstanceSnapshot, WireServiceMemberSnapshot, WireServiceProviderUpdate,
    WireServiceSubscriptionSnapshot,
};
pub use tracker::{Change, MAX_DELTA_OPERATIONS, Prepared, Tracker, track};
