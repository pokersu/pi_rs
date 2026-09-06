//! Rust 翻译自 packages/agent/src/harness/runtime/harness.ts
//!
//! AgentHarness 的运行时实现：管理 lanes，本身不是 lane。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_ai::models::Models;
use serde_json::Value as Json;

use crate::harness::agent_harness::{
    AgentHarnessApi, AgentLane, Closed, HarnessError, LaneSnapshot, OpenOperation, Resources,
};
use crate::harness::context::Context;
use crate::harness::events::{HarnessEventBus, WatchHandler};
use crate::harness::hooks::HookRegistry;
use crate::harness::runtime::lane::{FaultHandler, LaneImpl};
use crate::harness::runtime::restore::{
    ClassifiedLaneStorage, read_lane_storage, restore_lane_state, restore_session_arc,
};
use crate::harness::runtime::types::{Config, Lane, LaneRuntimeState};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{LaneConfiguration, ModelIdentity, Session};
use crate::harness::session::values::{
    branch_tip, lane_config as lane_config_value, lane_state as lane_state_value, set_value,
};

type EmitBatch = Arc<
    dyn Fn(
            Vec<crate::harness::agent_harness::HarnessEvent>,
            Context,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// 对应 `Harness`（runtime 实现）。
pub struct Harness {
    pub session: Arc<dyn Session>,
    pub models: Arc<Models>,
    pub hooks: Arc<HookRegistry>,
    pub events: Arc<HarnessEventBus>,
    lanes: Mutex<BTreeMap<String, Arc<LaneImpl>>>,
    seed: LaneConfiguration,
    config: Arc<Mutex<Config>>,
    closed_error: Mutex<Option<String>>,
    fault_error: Arc<Mutex<Option<String>>>,
}

impl Harness {
    fn assert_open(&self) -> Result<(), HarnessError> {
        if let Some(error) = &*self.fault_error.lock().unwrap() {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        if let Some(error) = &*self.closed_error.lock().unwrap() {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn fault(&self, cause: String, context: &Context) -> HarnessError {
        if let Some(error) = &*self.fault_error.lock().unwrap() {
            return HarnessError::Closed(Closed::new(error.clone()));
        }
        if let Some(error) = &*self.closed_error.lock().unwrap() {
            return HarnessError::Closed(Closed::new(error.clone()));
        }
        let fault = format!("AgentHarness storage or invariant fault: {cause}");
        *self.fault_error.lock().unwrap() = Some(fault.clone());
        for lane in self.lanes.lock().unwrap().values() {
            lane.seal(fault.clone());
        }
        self.hooks.close(fault.clone());
        let _ = context;
        HarnessError::Closed(Closed::new(fault))
    }

    fn build_lane(&self, name: String, state: LaneRuntimeState) -> Arc<LaneImpl> {
        let session = Arc::clone(&self.session);
        let models = Arc::clone(&self.models);
        let hooks = Arc::clone(&self.hooks);
        let events_for_emit = Arc::clone(&self.events);
        let events_for_watch = Arc::clone(&self.events);
        let config = Arc::clone(&self.config);
        let fault_lane = Arc::clone(&self.fault_error);
        let on_fault: FaultHandler = Arc::new(move |cause, _context| {
            fault_lane
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| format!("AgentHarness storage or invariant fault: {cause}"))
        });
        let emit_batch: EmitBatch = Arc::new(move |batch, context| {
            let bus = Arc::clone(&events_for_emit);
            Box::pin(async move {
                bus.emit_batch(batch, context).await;
            })
        });
        let install_watch: WatchHandler<LaneSnapshot> =
            Arc::new(move |snapshot, filter, context, resnapshot| {
                events_for_watch.watch(snapshot, filter, context, resnapshot)
            });
        let config_fn: Arc<dyn Fn() -> Config + Send + Sync> =
            Arc::new(move || config.lock().unwrap().clone());
        LaneImpl::new(
            name,
            session,
            models,
            hooks,
            state,
            on_fault,
            emit_batch,
            install_watch,
            config_fn,
        )
    }

    async fn get_name(&self, context: &Context) -> Result<Option<String>, HarnessError> {
        self.assert_open()?;
        self.session
            .get_name(context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn set_name(&self, name: Option<String>, context: &Context) -> Result<(), HarnessError> {
        self.assert_open()?;
        self.session
            .set_name(name.clone(), context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ValueUpdate {
                    payload: crate::harness::harness_event::ValueUpdatePayload::SessionName {
                        name,
                    },
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_label(
        &self,
        target_id: &str,
        context: &Context,
    ) -> Result<Option<String>, HarnessError> {
        self.assert_open()?;
        self.session
            .get_label(target_id, context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        self.session
            .set_label(target_id, label.clone(), context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ValueUpdate {
                    payload: crate::harness::harness_event::ValueUpdatePayload::EntryLabel {
                        target_id: target_id.to_string(),
                        label,
                    },
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_tools(
        &self,
        context: &Context,
    ) -> Result<Vec<crate::harness::types::AgentHarnessTool>, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().tools.clone())
    }

    async fn set_tools(
        &self,
        tools: Vec<crate::harness::types::AgentHarnessTool>,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        crate::harness::config::validate_tool_names(&names);
        self.config.lock().unwrap().tools = tools;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload: crate::harness::harness_event::ConfigUpdatePayload::Tools,
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_resources(&self, context: &Context) -> Result<Resources, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().resources.clone())
    }

    async fn set_resources(
        &self,
        resources: Resources,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        self.config.lock().unwrap().resources = resources;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload: crate::harness::harness_event::ConfigUpdatePayload::Resources,
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_stream_options(
        &self,
        context: &Context,
    ) -> Result<crate::harness::types::AgentHarnessStreamOptions, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().stream_options.clone())
    }

    async fn set_stream_options(
        &self,
        options: crate::harness::types::AgentHarnessStreamOptions,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        let previous = self.config.lock().unwrap().stream_options.clone();
        self.config.lock().unwrap().stream_options = options.clone();
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload: crate::harness::harness_event::ConfigUpdatePayload::StreamOptions {
                        value: options,
                        previous,
                    },
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_retry_policy(
        &self,
        context: &Context,
    ) -> Result<pi_ai::utils::retry::RetryPolicy, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().retry_policy)
    }

    async fn set_retry_policy(
        &self,
        policy: pi_ai::utils::retry::RetryPolicy,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        crate::harness::config::validate_retry_policy(&policy);
        let previous = self.config.lock().unwrap().retry_policy;
        self.config.lock().unwrap().retry_policy = policy;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload: crate::harness::harness_event::ConfigUpdatePayload::RetryPolicy {
                        value: policy,
                        previous,
                    },
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_compaction_settings(
        &self,
        context: &Context,
    ) -> Result<crate::harness::compaction::compaction::CompactionSettings, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().compaction.clone())
    }

    async fn set_compaction_settings(
        &self,
        settings: crate::harness::compaction::compaction::CompactionSettings,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        crate::harness::config::validate_compaction_settings(&settings);
        let previous = self.config.lock().unwrap().compaction.clone();
        self.config.lock().unwrap().compaction = settings.clone();
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload:
                        crate::harness::harness_event::ConfigUpdatePayload::CompactionSettings {
                            value: settings,
                            previous,
                        },
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_steering_mode(
        &self,
        context: &Context,
    ) -> Result<crate::types::QueueMode, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().steering_mode)
    }

    async fn set_steering_mode(
        &self,
        mode: crate::types::QueueMode,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        let previous = self.config.lock().unwrap().steering_mode;
        self.config.lock().unwrap().steering_mode = mode;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload: crate::harness::harness_event::ConfigUpdatePayload::SteeringMode {
                        value: mode,
                        previous,
                    },
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn get_follow_up_mode(
        &self,
        context: &Context,
    ) -> Result<crate::types::QueueMode, HarnessError> {
        self.assert_open()?;
        let _ = context;
        Ok(self.config.lock().unwrap().follow_up_mode)
    }

    async fn set_follow_up_mode(
        &self,
        mode: crate::types::QueueMode,
        context: &Context,
    ) -> Result<(), HarnessError> {
        self.assert_open()?;
        let previous = self.config.lock().unwrap().follow_up_mode;
        self.config.lock().unwrap().follow_up_mode = mode;
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::ConfigUpdate {
                    lane: None,
                    payload: crate::harness::harness_event::ConfigUpdatePayload::FollowUpMode {
                        value: mode,
                        previous,
                    },
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(())
    }

    async fn watch_session(&self, _context: &Context) -> Result<serde_json::Value, HarnessError> {
        self.assert_open()?;
        panic!(
            "{}",
            crate::harness::runtime::types::SliceNotImplemented {
                operation: "watchSession".to_string()
            }
        );
    }

    async fn get_or_create_lane(
        &self,
        name: &str,
        create_at: Option<String>,
        context: &Context,
    ) -> Result<Arc<LaneImpl>, HarnessError> {
        if name.is_empty() || name.contains('\u{0}') {
            return Err(HarnessError::InvalidLane(
                crate::harness::agent_harness::InvalidLane::new(
                    name.to_string(),
                    if name.is_empty() {
                        "lane name must not be empty".to_string()
                    } else {
                        "lane name must not contain \\u0000".to_string()
                    },
                    format!("Invalid lane {name:?}"),
                ),
            ));
        }
        if let Some(existing) = self.lanes.lock().unwrap().get(name).cloned() {
            return Ok(existing);
        }
        let stored = read_lane_storage(self.session.as_ref(), name, context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let lane = match &stored {
            ClassifiedLaneStorage::Lane { .. } => {
                let state = restore_lane_state(self.session.as_ref(), name, &stored, context)
                    .await
                    .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                self.build_lane(name.to_string(), state)
            }
            ClassifiedLaneStorage::Branch { tip } => {
                let tip_id: Option<String> = serde_json::from_value(tip.value.clone()).ok();
                let attached = self.seed.clone();
                let state = LaneRuntimeState {
                    tip_id: tip_id.clone(),
                    configuration: attached.clone(),
                    inbox: Vec::new(),
                    last_operation_id: None,
                    operation: None,
                };
                let writes = vec![
                    crate::harness::session::types::Write::Value(set_value(
                        &lane_config_value(name),
                        serde_json::to_value(&attached).unwrap_or(serde_json::Value::Null),
                    )),
                    crate::harness::session::types::Write::Value(set_value(
                        &lane_state_value(name),
                        serde_json::json!({
                            "currentOperationId": null,
                            "lastOperationId": null,
                            "inbox": [],
                        }),
                    )),
                ];
                let _ = tip_id;
                self.session
                    .begin_mutation(context)
                    .await
                    .map_err(|e| HarnessError::Closed(Closed::new(e)))?
                    .commit(writes, context)
                    .await
                    .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                self.build_lane(name.to_string(), state)
            }
            ClassifiedLaneStorage::Absent => {
                let tip_id = create_at;
                if let Some(tip_id) = &tip_id {
                    let found = self
                        .session
                        .get_entries(std::slice::from_ref(tip_id), context)
                        .await
                        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                    if !found.contains_key(tip_id) {
                        return Err(HarnessError::UnknownTarget(
                            crate::harness::agent_harness::UnknownTarget::new(
                                tip_id.clone(),
                                format!("Unknown target: {tip_id}"),
                            ),
                        ));
                    }
                }
                let attached = self.seed.clone();
                let state = LaneRuntimeState {
                    tip_id: tip_id.clone(),
                    configuration: attached.clone(),
                    inbox: Vec::new(),
                    last_operation_id: None,
                    operation: None,
                };
                let mut writes = Vec::new();
                if tip_id.is_none() {
                    writes.push(crate::harness::session::types::Write::Value(set_value(
                        &branch_tip(name),
                        serde_json::Value::Null,
                    )));
                }
                writes.push(crate::harness::session::types::Write::Value(set_value(
                    &lane_config_value(name),
                    serde_json::to_value(&attached).unwrap_or(serde_json::Value::Null),
                )));
                writes.push(crate::harness::session::types::Write::Value(set_value(
                    &lane_state_value(name),
                    serde_json::json!({
                        "currentOperationId": null,
                        "lastOperationId": null,
                        "inbox": [],
                    }),
                )));
                self.session
                    .begin_mutation(context)
                    .await
                    .map_err(|e| HarnessError::Closed(Closed::new(e)))?
                    .commit(writes, context)
                    .await
                    .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                self.build_lane(name.to_string(), state)
            }
        };
        self.lanes
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::clone(&lane));
        let lane_tip = self
            .lanes
            .lock()
            .unwrap()
            .get(name)
            .map(|l| l.state().tip_id)
            .unwrap_or(None);
        self.events
            .emit(
                crate::harness::harness_event::HarnessEvent::LaneCreated {
                    lane: name.to_string(),
                    at: lane_tip,
                    recovery: None,
                },
                context.clone(),
            )
            .await;
        Ok(lane)
    }
}

#[async_trait::async_trait]
impl AgentHarnessApi for Harness {
    async fn lane(
        &self,
        name: &str,
        context: &Context,
    ) -> Result<Option<Arc<dyn AgentLane>>, HarnessError> {
        self.assert_open()?;
        let lane = self.get_or_create_lane(name, None, context).await?;
        Ok(Some(lane))
    }

    async fn lanes(&self, context: &Context) -> Result<Vec<OpenOperation>, HarnessError> {
        self.assert_open()?;
        let mut result = Vec::new();
        for (name, lane) in self.lanes.lock().unwrap().iter() {
            let state = lane.state();
            if let Some(op) = state.operation {
                result.push(OpenOperation {
                    lane: name.clone(),
                    operation_id: op.meta.operation_id,
                    kind: crate::harness::runtime::drive::intent_kind(&op.meta.intent).to_string(),
                    started_at: op.meta.started_at,
                    aborting: matches!(
                        crate::harness::runtime::lane::state_control(&op.state),
                        crate::harness::session::types::Control::CancelRequested { .. }
                    )
                    .then_some(true),
                });
            }
        }
        let _ = context;
        Ok(result)
    }

    async fn close(&self, context: &Context) -> Result<(), HarnessError> {
        if let Some(error) = &*self.closed_error.lock().unwrap() {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        let error = "AgentHarness was closed while the operation was active".to_string();
        *self.closed_error.lock().unwrap() = Some(error.clone());
        for lane in self.lanes.lock().unwrap().values() {
            let _ = lane.seal(error.clone());
        }
        self.hooks.close(error.clone());
        self.session
            .close(context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(())
    }

    async fn get_name(&self, context: &Context) -> Result<Option<String>, HarnessError> {
        Harness::get_name(self, context).await
    }

    async fn set_name(&self, name: Option<String>, context: &Context) -> Result<(), HarnessError> {
        Harness::set_name(self, name, context).await
    }

    async fn get_label(
        &self,
        target_id: &str,
        context: &Context,
    ) -> Result<Option<String>, HarnessError> {
        Harness::get_label(self, target_id, context).await
    }

    async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_label(self, target_id, label, context).await
    }

    async fn get_tools(
        &self,
        context: &Context,
    ) -> Result<Vec<crate::harness::types::AgentHarnessTool>, HarnessError> {
        Harness::get_tools(self, context).await
    }

    async fn set_tools(
        &self,
        tools: Vec<crate::harness::types::AgentHarnessTool>,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_tools(self, tools, context).await
    }

    async fn get_resources(&self, context: &Context) -> Result<Resources, HarnessError> {
        Harness::get_resources(self, context).await
    }

    async fn set_resources(
        &self,
        resources: Resources,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_resources(self, resources, context).await
    }

    async fn get_stream_options(
        &self,
        context: &Context,
    ) -> Result<crate::harness::types::AgentHarnessStreamOptions, HarnessError> {
        Harness::get_stream_options(self, context).await
    }

    async fn set_stream_options(
        &self,
        options: crate::harness::types::AgentHarnessStreamOptions,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_stream_options(self, options, context).await
    }

    async fn get_retry_policy(
        &self,
        context: &Context,
    ) -> Result<pi_ai::utils::retry::RetryPolicy, HarnessError> {
        Harness::get_retry_policy(self, context).await
    }

    async fn set_retry_policy(
        &self,
        policy: pi_ai::utils::retry::RetryPolicy,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_retry_policy(self, policy, context).await
    }

    async fn get_compaction_settings(
        &self,
        context: &Context,
    ) -> Result<crate::harness::compaction::compaction::CompactionSettings, HarnessError> {
        Harness::get_compaction_settings(self, context).await
    }

    async fn set_compaction_settings(
        &self,
        settings: crate::harness::compaction::compaction::CompactionSettings,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_compaction_settings(self, settings, context).await
    }

    async fn get_steering_mode(
        &self,
        context: &Context,
    ) -> Result<crate::types::QueueMode, HarnessError> {
        Harness::get_steering_mode(self, context).await
    }

    async fn set_steering_mode(
        &self,
        mode: crate::types::QueueMode,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_steering_mode(self, mode, context).await
    }

    async fn get_follow_up_mode(
        &self,
        context: &Context,
    ) -> Result<crate::types::QueueMode, HarnessError> {
        Harness::get_follow_up_mode(self, context).await
    }

    async fn set_follow_up_mode(
        &self,
        mode: crate::types::QueueMode,
        context: &Context,
    ) -> Result<(), HarnessError> {
        Harness::set_follow_up_mode(self, mode, context).await
    }

    async fn watch_session(&self, context: &Context) -> Result<Json, HarnessError> {
        Harness::watch_session(self, context).await
    }
}

/// 对应 `AgentHarnessOptions`（harness.rs 内部使用的完整选项）。
pub struct HarnessOptions {
    pub session: Arc<dyn Session>,
    pub models: Arc<Models>,
    pub model: ModelIdentity,
    pub thinking_level: crate::types::ThinkingLevel,
    pub active_tool_names: Vec<String>,
    pub config: Config,
}

/// 对应 `createAgentHarness`。
pub async fn create_agent_harness(
    options: HarnessOptions,
    context: &Context,
) -> Result<(Arc<Harness>, Vec<OpenOperation>), HarnessError> {
    let seed = LaneConfiguration {
        model: options.model.clone(),
        thinking_level: options.thinking_level,
        active_tool_names: options.active_tool_names.clone(),
    };
    let restored = restore_session_arc(&options.session, context)
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
    let hooks = Arc::new(HookRegistry::new(Arc::new(
        |_error, _hook, _lane, _context| Box::pin(async move {}),
    )));
    let events = Arc::new(HarnessEventBus::new());
    let harness = Arc::new(Harness {
        session: options.session,
        models: options.models,
        hooks,
        events,
        lanes: Mutex::new(BTreeMap::new()),
        seed,
        config: Arc::new(Mutex::new(options.config)),
        closed_error: Mutex::new(None),
        fault_error: Arc::new(Mutex::new(None)),
    });

    let mut open = Vec::new();
    for (name, state) in restored {
        if let Some(op) = &state.operation {
            open.push(OpenOperation {
                lane: name.clone(),
                operation_id: op.meta.operation_id.clone(),
                kind: crate::harness::runtime::drive::intent_kind(&op.meta.intent).to_string(),
                started_at: op.meta.started_at,
                aborting: matches!(
                    crate::harness::runtime::lane::state_control(&op.state),
                    crate::harness::session::types::Control::CancelRequested { .. }
                )
                .then_some(true),
            });
        }
        let lane = harness.build_lane(name.clone(), state);
        harness.lanes.lock().unwrap().insert(name, lane);
    }
    Ok((harness, open))
}

// 保留引用以消除 unused（Resources / SessionInvariantError 等）。
#[allow(dead_code)]
fn _keep(_: &Resources, _: SessionInvariantError) {}
