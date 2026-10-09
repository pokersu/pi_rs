//! 对应 chord `src/services/state-codec.ts` 与 `src/services/wire.ts` 的类型层。
//!
//! 为服务订阅的每个复制状态维护一个有状态的 `Encoder`/`Decoder`（路径 interning），
//! 把未压缩的 `Op` 快照/更新转成线上 `WireOp` 形态，以及反向还原。
//!
//! # 与上游的差异
//!
//! - `wire.ts` 的 `parse*`/`assert*` 协议校验函数未移植：Rust 侧用 serde 反序列化，
//!   结构不对即报错（等价于上游的 `assertValidWireOp` 路径校验由 [`Decoder::decode`] 承担）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::delta::{Decoder, Encoder, Op, WireOp};

/// 对应 `ServiceMode = "singleton" | "keyed"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceMode {
    Singleton,
    Keyed,
}

/// 对应 `ServiceInstanceAddress`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInstanceAddress {
    pub key: String,
    pub generation: u64,
}

/// 对应 `ServiceMemberSnapshot`（未压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ServiceMemberSnapshot {
    Method {
        name: String,
    },
    State {
        name: String,
        sequence: u64,
        ops: Vec<Op>,
    },
}

/// 对应 `ServiceInstanceSnapshot`（未压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInstanceSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<ServiceInstanceAddress>,
    pub members: Vec<ServiceMemberSnapshot>,
}

/// 对应 `ServiceSubscriptionSnapshot`（未压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSubscriptionSnapshot {
    pub service_id: String,
    pub mode: ServiceMode,
    pub instances: Vec<ServiceInstanceSnapshot>,
}

/// 对应 `ServiceProviderUpdate`（未压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServiceProviderUpdate {
    State {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<ServiceInstanceAddress>,
        member: String,
        sequence: u64,
        ops: Vec<Op>,
    },
    Reset {
        snapshot: ServiceSubscriptionSnapshot,
    },
    Unavailable,
    Replaced {
        snapshot: ServiceInstanceSnapshot,
    },
    Spawned {
        instance: ServiceInstanceSnapshot,
    },
    Closed {
        instance: ServiceInstanceAddress,
    },
}

/// 对应 `WireServiceMemberSnapshot`（压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireServiceMemberSnapshot {
    Method {
        name: String,
    },
    State {
        name: String,
        sequence: u64,
        ops: Vec<WireOp>,
    },
}

/// 对应 `WireServiceInstanceSnapshot`（压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireServiceInstanceSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<ServiceInstanceAddress>,
    pub members: Vec<WireServiceMemberSnapshot>,
}

/// 对应 `WireServiceSubscriptionSnapshot`（压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireServiceSubscriptionSnapshot {
    pub service_id: String,
    pub mode: ServiceMode,
    pub instances: Vec<WireServiceInstanceSnapshot>,
}

/// 对应 `WireServiceProviderUpdate`（压缩）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WireServiceProviderUpdate {
    State {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<ServiceInstanceAddress>,
        member: String,
        sequence: u64,
        ops: Vec<WireOp>,
    },
    Reset {
        snapshot: WireServiceSubscriptionSnapshot,
    },
    Unavailable,
    Replaced {
        snapshot: WireServiceInstanceSnapshot,
    },
    Spawned {
        instance: WireServiceInstanceSnapshot,
    },
    Closed {
        instance: ServiceInstanceAddress,
    },
}

/// 对应 `stateKey`：把实例地址 + 成员名归一化为键。
fn state_key(instance: &Option<ServiceInstanceAddress>, member: &str) -> String {
    let key = instance.as_ref().map(|a| a.key.as_str());
    let generation = instance.as_ref().map(|a| a.generation);
    serde_json::json!([key, generation, member]).to_string()
}

/// 对应 `sameAddress`。
fn same_address(left: &Option<ServiceInstanceAddress>, right: &ServiceInstanceAddress) -> bool {
    left.as_ref()
        .map(|l| l.key == right.key && l.generation == right.generation)
        .unwrap_or(false)
}

/// 对应 `describeState`。
fn describe_state(instance: &Option<ServiceInstanceAddress>, member: &str) -> String {
    match instance {
        None => member.to_string(),
        Some(address) => format!("{}@{}.{}", address.key, address.generation, member),
    }
}

/// 对应 `StateCodecRegistry`：每个复制状态一个有状态编解码器。
struct StateCodecRegistry<C> {
    create: Box<dyn Fn() -> C>,
    entries: HashMap<String, (Option<ServiceInstanceAddress>, C)>,
}

impl<C> StateCodecRegistry<C> {
    fn new(create: Box<dyn Fn() -> C>) -> Self {
        Self {
            create,
            entries: HashMap::new(),
        }
    }

    /// 对应 `reset`。
    fn reset(&mut self) {
        self.entries.clear();
    }

    /// 对应 `add`：不存在则创建并返回；存在则 panic（对应上游 throw）。
    fn with_new_codec<R>(
        &mut self,
        instance: &Option<ServiceInstanceAddress>,
        member: &str,
        f: impl FnOnce(&mut C) -> R,
    ) -> R {
        let key = state_key(instance, member);
        if !self.entries.contains_key(&key) {
            let codec = (self.create)();
            self.entries.insert(key.clone(), (instance.clone(), codec));
        } else {
            panic!(
                "Duplicate service state {}",
                describe_state(instance, member)
            );
        }
        let (_, codec) = self.entries.get_mut(&key).expect("just inserted");
        f(codec)
    }

    /// 对应 `get`：必须已存在。
    fn with_existing_codec<R>(
        &mut self,
        instance: &Option<ServiceInstanceAddress>,
        member: &str,
        f: impl FnOnce(&mut C) -> R,
    ) -> R {
        let key = state_key(instance, member);
        let (_, codec) = self.entries.get_mut(&key).unwrap_or_else(|| {
            panic!("Unknown service state {}", describe_state(instance, member))
        });
        f(codec)
    }

    /// 对应 `removeInstance`。
    fn remove_instance(&mut self, instance: &ServiceInstanceAddress) {
        self.entries
            .retain(|_, (address, _)| !same_address(address, instance));
    }
}

/// 对应 `encodeInstance`：把一个实例快照的 state 成员 op 压缩。
fn encode_instance(
    instance: &ServiceInstanceSnapshot,
    codecs: &mut StateCodecRegistry<Encoder>,
) -> WireServiceInstanceSnapshot {
    let members = instance
        .members
        .iter()
        .map(|member| match member {
            ServiceMemberSnapshot::Method { name } => {
                WireServiceMemberSnapshot::Method { name: name.clone() }
            }
            ServiceMemberSnapshot::State {
                name,
                sequence,
                ops,
            } => {
                let ops =
                    codecs.with_new_codec(&instance.instance, name, |codec| codec.encode(ops));
                WireServiceMemberSnapshot::State {
                    name: name.clone(),
                    sequence: *sequence,
                    ops,
                }
            }
        })
        .collect();
    WireServiceInstanceSnapshot {
        instance: instance.instance.clone(),
        members,
    }
}

/// 对应 `decodeInstance`。
fn decode_instance(
    instance: &WireServiceInstanceSnapshot,
    codecs: &mut StateCodecRegistry<Decoder>,
) -> Result<ServiceInstanceSnapshot, crate::chord::DeltaError> {
    let mut members = Vec::with_capacity(instance.members.len());
    for member in &instance.members {
        match member {
            WireServiceMemberSnapshot::Method { name } => {
                members.push(ServiceMemberSnapshot::Method { name: name.clone() });
            }
            WireServiceMemberSnapshot::State {
                name,
                sequence,
                ops,
            } => {
                let ops =
                    codecs.with_new_codec(&instance.instance, name, |codec| codec.decode(ops))?;
                members.push(ServiceMemberSnapshot::State {
                    name: name.clone(),
                    sequence: *sequence,
                    ops,
                });
            }
        }
    }
    Ok(ServiceInstanceSnapshot {
        instance: instance.instance.clone(),
        members,
    })
}

/// 对应 `ServiceStateEncoder`：有状态的服务订阅编码器。
pub struct ServiceStateEncoder {
    codecs: StateCodecRegistry<Encoder>,
}

impl ServiceStateEncoder {
    /// 对应 `createServiceStateEncoder()`。
    pub fn new() -> Self {
        Self {
            codecs: StateCodecRegistry::new(Box::new(Encoder::new)),
        }
    }

    /// 对应 `encodeSnapshot`。
    pub fn encode_snapshot(
        &mut self,
        snapshot: &ServiceSubscriptionSnapshot,
    ) -> WireServiceSubscriptionSnapshot {
        self.codecs.reset();
        let instances = snapshot
            .instances
            .iter()
            .map(|instance| encode_instance(instance, &mut self.codecs))
            .collect();
        WireServiceSubscriptionSnapshot {
            service_id: snapshot.service_id.clone(),
            mode: snapshot.mode,
            instances,
        }
    }

    /// 对应 `encodeUpdate`。
    pub fn encode_update(&mut self, update: &ServiceProviderUpdate) -> WireServiceProviderUpdate {
        match update {
            ServiceProviderUpdate::State {
                instance,
                member,
                sequence,
                ops,
            } => {
                let ops = self
                    .codecs
                    .with_existing_codec(instance, member, |codec| codec.encode(ops));
                WireServiceProviderUpdate::State {
                    instance: instance.clone(),
                    member: member.clone(),
                    sequence: *sequence,
                    ops,
                }
            }
            ServiceProviderUpdate::Reset { snapshot } => {
                self.codecs.reset();
                let instances = snapshot
                    .instances
                    .iter()
                    .map(|instance| encode_instance(instance, &mut self.codecs))
                    .collect();
                WireServiceProviderUpdate::Reset {
                    snapshot: WireServiceSubscriptionSnapshot {
                        service_id: snapshot.service_id.clone(),
                        mode: snapshot.mode,
                        instances,
                    },
                }
            }
            ServiceProviderUpdate::Unavailable => {
                self.codecs.reset();
                WireServiceProviderUpdate::Unavailable
            }
            ServiceProviderUpdate::Replaced { snapshot } => {
                self.codecs.reset();
                WireServiceProviderUpdate::Replaced {
                    snapshot: encode_instance(snapshot, &mut self.codecs),
                }
            }
            ServiceProviderUpdate::Spawned { instance } => WireServiceProviderUpdate::Spawned {
                instance: encode_instance(instance, &mut self.codecs),
            },
            ServiceProviderUpdate::Closed { instance } => {
                self.codecs.remove_instance(instance);
                WireServiceProviderUpdate::Closed {
                    instance: instance.clone(),
                }
            }
        }
    }
}

impl Default for ServiceStateEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// 对应 `ServiceStateDecoder`：有状态的服务订阅解码器。
pub struct ServiceStateDecoder {
    codecs: StateCodecRegistry<Decoder>,
}

impl ServiceStateDecoder {
    /// 对应 `createServiceStateDecoder()`。
    pub fn new() -> Self {
        Self {
            codecs: StateCodecRegistry::new(Box::new(Decoder::new)),
        }
    }

    /// 对应 `decodeSnapshot`。
    pub fn decode_snapshot(
        &mut self,
        snapshot: &WireServiceSubscriptionSnapshot,
    ) -> Result<ServiceSubscriptionSnapshot, crate::chord::DeltaError> {
        self.codecs.reset();
        let mut instances = Vec::with_capacity(snapshot.instances.len());
        for instance in &snapshot.instances {
            instances.push(decode_instance(instance, &mut self.codecs)?);
        }
        Ok(ServiceSubscriptionSnapshot {
            service_id: snapshot.service_id.clone(),
            mode: snapshot.mode,
            instances,
        })
    }

    /// 对应 `decodeUpdate`。
    pub fn decode_update(
        &mut self,
        update: &WireServiceProviderUpdate,
    ) -> Result<ServiceProviderUpdate, crate::chord::DeltaError> {
        match update {
            WireServiceProviderUpdate::State {
                instance,
                member,
                sequence,
                ops,
            } => {
                let ops = self
                    .codecs
                    .with_existing_codec(instance, member, |codec| codec.decode(ops))?;
                Ok(ServiceProviderUpdate::State {
                    instance: instance.clone(),
                    member: member.clone(),
                    sequence: *sequence,
                    ops,
                })
            }
            WireServiceProviderUpdate::Reset { snapshot } => {
                self.codecs.reset();
                let mut instances = Vec::with_capacity(snapshot.instances.len());
                for instance in &snapshot.instances {
                    instances.push(decode_instance(instance, &mut self.codecs)?);
                }
                Ok(ServiceProviderUpdate::Reset {
                    snapshot: ServiceSubscriptionSnapshot {
                        service_id: snapshot.service_id.clone(),
                        mode: snapshot.mode,
                        instances,
                    },
                })
            }
            WireServiceProviderUpdate::Unavailable => {
                self.codecs.reset();
                Ok(ServiceProviderUpdate::Unavailable)
            }
            WireServiceProviderUpdate::Replaced { snapshot } => {
                self.codecs.reset();
                Ok(ServiceProviderUpdate::Replaced {
                    snapshot: decode_instance(snapshot, &mut self.codecs)?,
                })
            }
            WireServiceProviderUpdate::Spawned { instance } => Ok(ServiceProviderUpdate::Spawned {
                instance: decode_instance(instance, &mut self.codecs)?,
            }),
            WireServiceProviderUpdate::Closed { instance } => {
                self.codecs.remove_instance(instance);
                Ok(ServiceProviderUpdate::Closed {
                    instance: instance.clone(),
                })
            }
        }
    }
}

impl Default for ServiceStateDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::delta::PathSegment;
    use serde_json::json;

    fn key(name: &str) -> PathSegment {
        PathSegment::Key(name.to_string())
    }

    fn snapshot() -> ServiceSubscriptionSnapshot {
        ServiceSubscriptionSnapshot {
            service_id: "svc".to_string(),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: None,
                members: vec![
                    ServiceMemberSnapshot::Method {
                        name: "ping".to_string(),
                    },
                    ServiceMemberSnapshot::State {
                        name: "counter".to_string(),
                        sequence: 1,
                        ops: vec![
                            Op::Replace(json!({ "n": 0 })),
                            Op::Set(vec![key("n")], json!(1)),
                            Op::Set(vec![key("tag")], json!("x")),
                            Op::Set(vec![key("n")], json!(2)),
                        ],
                    },
                ],
            }],
        }
    }

    #[test]
    fn snapshot_roundtrip_preserves_ops() {
        let original = snapshot();
        let mut encoder = ServiceStateEncoder::new();
        let wire = encoder.encode_snapshot(&original);
        let mut decoder = ServiceStateDecoder::new();
        let decoded = decoder.decode_snapshot(&wire).unwrap();
        assert_eq!(decoded, original, "快照 encode→decode 应还原");
    }

    #[test]
    fn update_roundtrip_reuses_codec_state() {
        let original = snapshot();
        let mut encoder = ServiceStateEncoder::new();
        let wire_snapshot = encoder.encode_snapshot(&original);

        let update = ServiceProviderUpdate::State {
            instance: None,
            member: "counter".to_string(),
            sequence: 2,
            ops: vec![Op::Set(vec![key("n")], json!(3))],
        };
        let wire_update = encoder.encode_update(&update);

        let mut decoder = ServiceStateDecoder::new();
        let _ = decoder.decode_snapshot(&wire_snapshot).unwrap();
        let decoded_update = decoder.decode_update(&wire_update).unwrap();
        assert_eq!(decoded_update, update, "更新 encode→decode 应还原");
    }
}
