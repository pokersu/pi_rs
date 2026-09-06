// structural runStructural* 系列（追加到 structural.rs）

/// 对应 `summaryKind`。
fn summary_kind(task: &SummaryTask) -> &'static str {
    if matches!(
        task.boundary,
        crate::harness::session::types::ResultBoundary::CommitNavigation { .. }
    ) {
        "branch_summary"
    } else {
        "compaction"
    }
}

/// 对应 `compactionReason`。
fn compaction_reason(task: &SummaryTask) -> String {
    if let Some(reason) = &task.reason {
        return reason.clone();
    }
    if matches!(task.boundary, crate::harness::session::types::ResultBoundary::Finish) {
        return "manual".to_string();
    }
    panic!(
        "{}",
        SessionInvariantError::new(format!(
            "In-run compaction task {} is missing its reason",
            task.task_id
        ))
    )
}

/// 对应 `operationError`。
fn operation_error(code: &str, message: &str, details: Option<serde_json::Value>) -> OperationError {
    OperationError {
        code: code.to_string(),
        message: message.to_string(),
        details,
    }
}

/// 对应 `summaryContext`。
fn summary_context(
    config: &crate::harness::runtime::types::Config,
    retry: pi_ai::utils::retry::RetryPolicy,
    result_entry_id: String,
    configuration: crate::harness::session::types::LaneConfiguration,
) -> SummaryContext {
    let mut stream_options = config.stream_options.clone();
    stream_options.deferred = Some(serde_json::json!(false));
    SummaryContext {
        result_entry_id,
        configuration,
        stream_options,
        retry_policy: normalized_retry_policy(&retry),
    }
}

/// 对应 `StructuralOutcome`。
enum StructuralOutcome {
    Compaction {
        result_entry_id: String,
        result: CompactResult,
        from_hook: bool,
    },
    BranchSummary {
        result_entry_id: String,
        result: BranchSummaryResult,
        from_hook: bool,
    },
    Declined,
    Failed { error: OperationError },
}

fn outcome_usage(outcome: &StructuralOutcome) -> Option<pi_ai::Usage> {
    match outcome {
        StructuralOutcome::Compaction { result, .. } => result.usage.clone(),
        StructuralOutcome::BranchSummary { result, .. } => result.usage.clone(),
        _ => None,
    }
}

async fn read_structural_preparation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deciding: &OperationState,
) -> Result<ContinueOperationResult<DurableStructuralPreparation>, String> {
    let task = match deciding {
        OperationState::SummaryDeciding { task, .. } => task.clone(),
        _ => unreachable!(),
    };
    let expected = summary_kind(&task).to_string();
    let operation_id = drive.operation_id.clone();
    let task_id = task.task_id.clone();
    let drive_context = drive.context.clone();
    lane.continue_operation(
        deciding,
        Box::new(move |_state, current, _meta, reader| {
            let operation_id = operation_id.clone();
            let task_id = task_id.clone();
            let context = drive_context.clone();
            let expected = expected.clone();
            Box::pin(async move {
                let stored = reader
                    .get_value(&operation_preparation(&operation_id, &task_id).erased(), &context)
                    .await
                    .expect("get_value");
                let stored = stored.unwrap_or_else(|| {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Structural task {task_id} is missing its {expected} preparation"
                        ))
                    )
                });
                let preparation: DurableStructuralPreparation = serde_json::from_value(
                    stored.value,
                )
                .unwrap_or_else(|e| panic!("parse structural preparation: {e}"));
                let current_task = match &current {
                    OperationState::SummaryDeciding { task, .. } => task,
                    _ => unreachable!(),
                };
                if summary_kind(current_task) != expected {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Structural task {task_id} is missing its {expected} preparation"
                        ))
                    );
                }
                OperationCommand::Return {
                    result: preparation,
                }
            })
        }),
        &drive.context,
    )
    .await
}

/// 对应 `publishStructuralReady`。
async fn publish_structural_ready<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deciding: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let result_entry_id = lane.session().id_generator().next(None);
    let config = lane.read_config();
    let retry = config.retry_policy;

    let published = lane
        .continue_operation(
            deciding,
            Box::new(move |state, current, _meta, _reader| {
                let result_entry_id = result_entry_id.clone();
                let config = config.clone();
                let retry = retry;
                Box::pin(async move {
                    let task = match &current {
                        OperationState::SummaryDeciding { task, .. } => task.clone(),
                        _ => unreachable!(),
                    };
                    let next = OperationState::SummaryReady {
                        scope: current.scope().clone(),
                        task,
                        summary_context: summary_context(
                            &config,
                            retry,
                            result_entry_id,
                            state.configuration.clone(),
                        ),
                        next_attempt: 1,
                    };
                    OperationCommand::Commit {
                        writes: Vec::new(),
                        operation_state: next,
                        lane: None,
                        materialize: Box::new(|_| ProcedureResult::Continue),
                        events: None,
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    Ok(match published {
        ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
        ContinueOperationResult::Result { value } => value,
    })
}

/// 对应 `publishStructuralOutcome`。
#[allow(clippy::too_many_arguments)]
async fn publish_structural_outcome<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
    outcome: StructuralOutcome,
) -> Result<ProcedureResult, GateError> {
    let hook_usage_id = match &outcome {
        StructuralOutcome::Compaction {
            from_hook: true,
            result,
            ..
        } if result.usage.is_some() => Some(lane.session().id_generator().next(None)),
        StructuralOutcome::BranchSummary {
            from_hook: true,
            result,
            ..
        } if result.usage.is_some() => Some(lane.session().id_generator().next(None)),
        _ => None,
    };

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let outcome = Arc::new(outcome);

    let published = lane
        .continue_operation(
            capability,
            Box::new(move |state, current, meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = drive_context.clone();
                let hook_usage_id = hook_usage_id.clone();
                let outcome = Arc::clone(&outcome);
                Box::pin(async move {
                    let task = match &current {
                        OperationState::SummaryDeciding { task, .. }
                        | OperationState::SummaryReady { task, .. }
                        | OperationState::SummaryEffectPending { task, .. }
                        | OperationState::SummaryRetryWait { task, .. } => task.clone(),
                        _ => unreachable!(),
                    };
                    let expected = summary_kind(&task);
                    if let StructuralOutcome::Compaction { .. } = &*outcome
                        && expected != "compaction"
                    {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "Structural compaction result does not match {expected} task {}",
                                task.task_id
                            ))
                        );
                    }
                    if let StructuralOutcome::BranchSummary { .. } = &*outcome
                        && expected != "branch_summary"
                    {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "Structural branch_summary result does not match {expected} task {}",
                                task.task_id
                            ))
                        );
                    }

                    let mut writes: Vec<Write> = Vec::new();
                    let mut terminal_tip_id = state.tip_id.clone();
                    if let Some(hook_usage_id) = &hook_usage_id {
                        let usage = outcome_usage(&outcome)
                            .expect("Hook usage id exists without structural usage");
                        let row = UsageRow {
                            id: hook_usage_id.clone(),
                            seq: 0,
                            usage,
                            entry_id: None,
                            adjustment: false,
                            details: None,
                        };
                        writes.push(insert_usage(row));
                    }

                    match &*outcome {
                        StructuralOutcome::Compaction {
                            result_entry_id,
                            result,
                            from_hook,
                        } => {
                            let entry = NewEntry::Compaction(NewCompactionEntry {
                                id: result_entry_id.clone(),
                                parent_id: state.tip_id.clone(),
                                custom_type: None,
                                summary: result.summary.clone(),
                                retained_tail: result.retained_tail.clone(),
                                tokens_before: result.tokens_before,
                                details: Some(serde_json::to_value(&result.details).unwrap_or(serde_json::Value::Null)),
                                usage: result.usage.clone(),
                                from_hook: *from_hook,
                            });
                            let entry_write_index = writes.len();
                            writes.push(insert_entry(entry.clone()));
                            writes.push(Write::Value(set_value(
                                &branch_tip(&lane_name),
                                serde_json::json!(result_entry_id),
                            )));
                            terminal_tip_id = Some(result_entry_id.clone());
                            let _ = entry_write_index;
                        }
                        StructuralOutcome::BranchSummary {
                            result_entry_id,
                            result,
                            from_hook,
                        } => {
                            let boundary = task.boundary.clone();
                            let target_id = match &boundary {
                                crate::harness::session::types::ResultBoundary::CommitNavigation {
                                    target_id,
                                    ..
                                } => target_id.clone(),
                                _ => unreachable!(),
                            };
                            let entry = NewEntry::BranchSummary(
                                crate::harness::session::types::NewBranchSummaryEntry {
                                    id: result_entry_id.clone(),
                                    parent_id: Some(target_id.clone()),
                                    custom_type: None,
                                    from_id: meta.source_tip_id.clone(),
                                    summary: result.summary.clone(),
                                    details: Some(serde_json::json!({
                                        "readFiles": result.read_files,
                                        "modifiedFiles": result.modified_files,
                                    })),
                                    usage: result.usage.clone(),
                                    from_hook: *from_hook,
                                },
                            );
                            writes.push(Write::Value(set_value(
                                &branch_tip(&lane_name),
                                serde_json::json!(target_id),
                            )));
                            writes.push(insert_entry(entry));
                            writes.push(Write::Value(set_value(
                                &branch_tip(&lane_name),
                                serde_json::json!(result_entry_id),
                            )));
                            if let Some(label) = match &boundary {
                                crate::harness::session::types::ResultBoundary::CommitNavigation {
                                    label,
                                    ..
                                } => label.clone(),
                                _ => None,
                            } {
                                writes.push(Write::Value(set_value(
                                    &entry_label(&target_id),
                                    serde_json::json!(label),
                                )));
                            }
                            terminal_tip_id = Some(result_entry_id.clone());
                        }
                        StructuralOutcome::Declined | StructuralOutcome::Failed { .. } => {}
                    }

                    let attempt = match &current {
                        OperationState::SummaryReady { next_attempt, .. } => Some(*next_attempt),
                        OperationState::SummaryEffectPending { attempt, .. } => Some(*attempt),
                        _ => None,
                    };
                    let _ = attempt;

                    // 终态分类：根据 boundary 决定 finish（declined/failed/completed）。
                    let status = match &*outcome {
                        StructuralOutcome::Declined => {
                            crate::harness::session::types::TerminalStatus::Declined
                        }
                        StructuralOutcome::Failed { .. } => {
                            crate::harness::session::types::TerminalStatus::Failed
                        }
                        _ => crate::harness::session::types::TerminalStatus::Completed,
                    };
                    let error = match &*outcome {
                        StructuralOutcome::Failed { error } => Some(error.clone()),
                        _ => None,
                    };
                    let cleanup = operation_cleanup_writes(
                        reader.as_ref(),
                        &operation_id,
                        &current,
                        &context,
                    )
                    .await
                    .expect("operation_cleanup_writes failed");
                    writes.extend(cleanup);
                    let record = operation_result_record(&meta, status, terminal_tip_id.clone(), error.clone());
                    let record_for_materialize = record.clone();
                    let record_for_events = record.clone();
                    let error_for_events = error.clone();
                    let status_for_events = status;
                    let terminal_tip_id_for_events = terminal_tip_id.clone();
                    let source_tip_id_for_events = meta.source_tip_id.clone();
                    let is_navigation = matches!(
                        task.boundary,
                        crate::harness::session::types::ResultBoundary::CommitNavigation { .. }
                    );
                    let reason_for_events = compaction_reason(&task);

                    OperationCommand::Finish {
                        writes,
                        record,
                        lane: Some(LanePatch {
                            tip_id: terminal_tip_id,
                            configuration: None,
                            inbox: None,
                        }),
                        materialize: Box::new(move |_| ProcedureResult::Settled {
                            outcome: record_for_materialize,
                        }),
                        events: Some(Box::new(move |_| {
                            if is_navigation {
                                vec![HarnessEvent::NavigationEnd {
                                    lane: lane_name,
                                    run_id: operation_id,
                                    from_tip_id: source_tip_id_for_events,
                                    tip_id: terminal_tip_id_for_events,
                                    ended_at: record_for_events.ended_at,
                                    status: status_for_events,
                                    error: error_for_events,
                                    recovery: None,
                                }]
                            } else {
                                vec![HarnessEvent::CompactionEnd {
                                    lane: lane_name,
                                    run_id: operation_id,
                                    reason: reason_for_events,
                                    ended_at: record_for_events.ended_at,
                                    status: status_for_events,
                                    entry_id: None,
                                    error: error_for_events,
                                    recovery: None,
                                }]
                            }
                        })),
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    Ok(match published {
        ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
        ContinueOperationResult::Result { value } => value,
    })
}

/// 对应 `runStructuralDecision`。
pub async fn run_structural_decision<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deciding: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let preparation = read_structural_preparation(lane, drive, deciding)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: preparation } = preparation else {
        return Ok(ProcedureResult::Continue);
    };
    let task = match deciding {
        OperationState::SummaryDeciding { task, .. } => task.clone(),
        _ => unreachable!(),
    };
    let navigation = matches!(
        task.boundary,
        crate::harness::session::types::ResultBoundary::CommitNavigation { .. }
    );

    let (hook_name, hook_key) = if navigation {
        (crate::harness::hooks::HookName::BeforeNavigation, "summary")
    } else {
        (crate::harness::hooks::HookName::BeforeCompaction, "compaction")
    };
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let hook = lane
        .hooks()
        .run_with_gate(
            hook_name,
            serde_json::json!({
                "lane": lane_name,
                "runId": operation_id,
                "reason": if navigation { None } else { Some(compaction_reason(&task)) },
                "preparation": preparation,
                "customInstructions": task.custom_instructions,
            }),
            &drive.gate,
            &drive.context,
        )
        .await?;
    if hook.get("decline").and_then(|v| v.as_bool()).unwrap_or(false) {
        return publish_structural_outcome(lane, drive, deciding, StructuralOutcome::Declined).await;
    }
    if let Some(hook_result) = hook.get(hook_key) {
        if navigation {
            if let Ok(result) = serde_json::from_value::<BranchSummaryResult>(hook_result.clone()) {
                return publish_structural_outcome(
                    lane,
                    drive,
                    deciding,
                    StructuralOutcome::BranchSummary {
                        result_entry_id: lane.session().id_generator().next(None),
                        result,
                        from_hook: true,
                    },
                )
                .await;
            }
        } else if let Ok(result) = serde_json::from_value::<CompactResult>(hook_result.clone()) {
            return publish_structural_outcome(
                lane,
                drive,
                deciding,
                StructuralOutcome::Compaction {
                    result_entry_id: lane.session().id_generator().next(None),
                    result,
                    from_hook: true,
                },
            )
            .await;
        }
    }
    publish_structural_ready(lane, drive, deciding).await
}

/// 对应 `effectPendingFromReady`。
fn effect_pending_from_ready(ready: &OperationState) -> OperationState {
    let (scope, task, summary_context, next_attempt) = match ready {
        OperationState::SummaryReady {
            scope,
            task,
            summary_context,
            next_attempt,
        } => (scope.clone(), task.clone(), summary_context.clone(), *next_attempt),
        _ => unreachable!(),
    };
    OperationState::SummaryEffectPending {
        scope,
        task,
        summary_context,
        attempt: next_attempt,
        request: None,
        usage_ids: Vec::new(),
    }
}

/// 对应 `retryWaitFromEffect`。
fn retry_wait_from_effect(effect: &OperationState, error_message: &str) -> OperationState {
    let (scope, task, summary_context, attempt) = match effect {
        OperationState::SummaryEffectPending {
            scope,
            task,
            summary_context,
            attempt,
            ..
        } => (scope.clone(), task.clone(), summary_context.clone(), *attempt),
        _ => unreachable!(),
    };
    OperationState::SummaryRetryWait {
        scope,
        task,
        summary_context,
        next_attempt: attempt + 1,
        not_before: retry_not_before(
            summary_context_for(effect).retry_policy.base_delay_ms,
            attempt,
            pi_ai::utils::uuid::now_ms() as u64,
        ),
        error_message: error_message.to_string(),
    }
}

fn summary_context_for(effect: &OperationState) -> SummaryContext {
    match effect {
        OperationState::SummaryEffectPending { summary_context, .. } => summary_context.clone(),
        OperationState::SummaryReady { summary_context, .. } => summary_context.clone(),
        OperationState::SummaryRetryWait { summary_context, .. } => summary_context.clone(),
        _ => unreachable!(),
    }
}

/// 对应 `publishAttemptIntent`。
async fn publish_attempt_intent<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    ready: &OperationState,
) -> Result<ContinueOperationResult<OperationState>, String> {
    lane.continue_operation(
        ready,
        Box::new(move |_state, current, _meta, _reader| {
            Box::pin(async move {
                let effect_pending = effect_pending_from_ready(&current);
                OperationCommand::Commit {
                    writes: Vec::new(),
                    operation_state: effect_pending.clone(),
                    lane: None,
                    materialize: Box::new(move |_| effect_pending),
                    events: None,
                }
            })
        }),
        &drive.context,
    )
    .await
}

/// 对应 `AttemptPreparation`（`CompactionPreparation | BranchPreparation` 联合）。
enum AttemptPreparation {
    Compaction(CompactionPreparation),
    BranchSummary(BranchPreparation),
}

/// 对应 `performStructuralAttempt` 的返回联合。
enum StructuralAttempt {
    Compaction { result: CompactResult },
    BranchSummary { result: BranchSummaryResult },
    Error { error: OperationError, retryable: bool },
}

/// 对应 `readAttemptPreparation`。
async fn read_attempt_preparation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    ready: &OperationState,
) -> Result<ContinueOperationResult<AttemptPreparation>, String> {
    let task = match ready {
        OperationState::SummaryReady { task, .. } => task.clone(),
        _ => unreachable!(),
    };
    let expected = summary_kind(&task).to_string();
    let operation_id = drive.operation_id.clone();
    let task_id = task.task_id.clone();
    let drive_context = drive.context.clone();
    lane.continue_operation(
        ready,
        Box::new(move |_state, current, _meta, reader| {
            let operation_id = operation_id.clone();
            let task_id = task_id.clone();
            let context = drive_context.clone();
            let expected = expected.clone();
            Box::pin(async move {
                let stored = reader
                    .get_value(&operation_preparation(&operation_id, &task_id).erased(), &context)
                    .await
                    .expect("get_value");
                let stored = stored.unwrap_or_else(|| {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Structural task {task_id} has invalid durable preparation"
                        ))
                    )
                });
                let preparation: DurableStructuralPreparation =
                    serde_json::from_value(stored.value)
                        .unwrap_or_else(|e| panic!("parse structural preparation: {e}"));
                let current_task = match &current {
                    OperationState::SummaryReady { task, .. } => task,
                    _ => unreachable!(),
                };
                if summary_kind(current_task) != expected {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Structural task {task_id} has invalid durable preparation"
                        ))
                    );
                }
                let attempt = if expected == "compaction" {
                    AttemptPreparation::Compaction(
                        compaction_preparation(&preparation).expect("compaction preparation"),
                    )
                } else {
                    AttemptPreparation::BranchSummary(
                        branch_preparation(&preparation).expect("branch preparation"),
                    )
                };
                OperationCommand::Return { result: attempt }
            })
        }),
        &drive.context,
    )
    .await
}

/// 对应 `performStructuralAttempt`。
async fn perform_structural_attempt<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    effect: &OperationState,
    model: &Model,
    preparation: &AttemptPreparation,
) -> Result<StructuralAttempt, GateError> {
    let (task, summary_context) = match effect {
        OperationState::SummaryEffectPending {
            task,
            summary_context,
            ..
        } => (task.clone(), summary_context.clone()),
        _ => unreachable!(),
    };
    let models = lane.models().clone();
    let drive_context = drive.context.clone();
    let model = model.clone();

    let last_response = Arc::new(std::sync::Mutex::new(None::<AssistantMessage>));
    let request_model = model.clone();
    let request_last_response = Arc::clone(&last_response);

    let request: SummaryRequest = Arc::new(move |ai_context, options, _ctx| {
        let models = models.clone();
        let model = request_model.clone();
        let last_response = Arc::clone(&request_last_response);
        Box::pin(async move {
            let response = models.complete_simple(model, ai_context, Some(options)).await;
            *last_response.lock().unwrap() = Some(response.clone());
            response
        })
    });

    let retryable = |last_response: &Arc<std::sync::Mutex<Option<AssistantMessage>>>| {
        last_response
            .lock()
            .unwrap()
            .as_ref()
            .map(is_retryable_assistant_error)
            .unwrap_or(false)
    };

    let result = match preparation {
        AttemptPreparation::Compaction(prep) => {
            compact_with_request(
                prep,
                &CompactGenerationOptions {
                    model: model.clone(),
                    custom_instructions: task.custom_instructions.clone(),
                    thinking_level: Some(summary_context.configuration.thinking_level),
                },
                &request,
                &drive_context,
            )
            .await
            .map(|r| StructuralAttempt::Compaction { result: r })
            .unwrap_or_else(|e| StructuralAttempt::Error {
                error: operation_error(
                    match e.code {
                        crate::harness::types::CompactionErrorCode::Aborted => "aborted",
                        crate::harness::types::CompactionErrorCode::SummarizationFailed => {
                            "summarization_failed"
                        }
                    },
                    &e.message,
                    None,
                ),
                retryable: retryable(&last_response),
            })
        }
        AttemptPreparation::BranchSummary(prep) => {
            generate_branch_summary_with_request(
                prep,
                &PreparedBranchSummaryOptions {
                    custom_instructions: task.custom_instructions.clone(),
                    replace_instructions: false,
                },
                &request,
                &drive_context,
            )
            .await
            .map(|r| StructuralAttempt::BranchSummary { result: r })
            .unwrap_or_else(|e| StructuralAttempt::Error {
                error: operation_error(
                    match e.code {
                        crate::harness::types::BranchSummaryErrorCode::Aborted => "aborted",
                        crate::harness::types::BranchSummaryErrorCode::SummarizationFailed => {
                            "summarization_failed"
                        }
                    },
                    &e.message,
                    None,
                ),
                retryable: retryable(&last_response),
            })
        }
    };
    Ok(result)
}

/// 对应 `publishAttemptResult`。
async fn publish_attempt_result<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    effect: &OperationState,
    result: StructuralAttempt,
) -> Result<ProcedureResult, GateError> {
    let (summary_context, attempt) = match effect {
        OperationState::SummaryEffectPending {
            summary_context,
            attempt,
            ..
        } => (summary_context.clone(), *attempt),
        _ => unreachable!(),
    };
    match result {
        StructuralAttempt::Compaction { result } => {
            publish_structural_outcome(
                lane,
                drive,
                effect,
                StructuralOutcome::Compaction {
                    result_entry_id: summary_context.result_entry_id,
                    result,
                    from_hook: false,
                },
            )
            .await
        }
        StructuralAttempt::BranchSummary { result } => {
            publish_structural_outcome(
                lane,
                drive,
                effect,
                StructuralOutcome::BranchSummary {
                    result_entry_id: summary_context.result_entry_id,
                    result,
                    from_hook: false,
                },
            )
            .await
        }
        StructuralAttempt::Error { error, retryable } => {
            if retryable && attempt < summary_context.retry_policy.max_attempts {
                let retry_wait = retry_wait_from_effect(effect, &error.message);
                let published = lane
                    .continue_operation(
                        effect,
                        Box::new(move |_state, _current, _meta, _reader| {
                            Box::pin(async move {
                                OperationCommand::Commit {
                                    writes: Vec::new(),
                                    operation_state: retry_wait,
                                    lane: None,
                                    materialize: Box::new(|_| ProcedureResult::Continue),
                                    events: None,
                                }
                            })
                        }),
                        &drive.context,
                    )
                    .await
                    .map_err(GateError::Closed)?;
                Ok(match published {
                    ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
                    ContinueOperationResult::Result { value } => value,
                })
            } else {
                publish_structural_outcome(lane, drive, effect, StructuralOutcome::Failed { error })
                    .await
            }
        }
    }
}

/// 对应 `runStructuralGeneration`。
pub async fn run_structural_generation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    ready: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let summary_context = match ready {
        OperationState::SummaryReady { summary_context, .. } => summary_context.clone(),
        _ => unreachable!(),
    };
    let preparation = read_attempt_preparation(lane, drive, ready)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: preparation } = preparation else {
        return Ok(ProcedureResult::Continue);
    };
    let identity = &summary_context.configuration.model;
    let model = lane.models().get_model(&identity.provider, &identity.model_id);
    let Some(model) = model else {
        return publish_structural_outcome(
            lane,
            drive,
            ready,
            StructuralOutcome::Failed {
                error: operation_error(
                    "model_unavailable",
                    "The configured model is unavailable in this process",
                    Some(serde_json::to_value(identity).unwrap_or(serde_json::Value::Null)),
                ),
            },
        )
        .await;
    };
    let intent = publish_attempt_intent(lane, drive, ready)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: intent } = intent else {
        return Ok(ProcedureResult::Continue);
    };
    let result = perform_structural_attempt(lane, drive, &intent, &model, &preparation).await?;
    publish_attempt_result(lane, drive, &intent, result).await
}

/// 对应 `runStructuralRetryWait`。
pub async fn run_structural_retry_wait<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    retry: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let (task, summary_context, next_attempt, not_before) = match retry {
        OperationState::SummaryRetryWait {
            task,
            summary_context,
            next_attempt,
            not_before,
            ..
        } => (task.clone(), summary_context.clone(), *next_attempt, *not_before),
        _ => return Ok(ProcedureResult::Continue),
    };
    let now = pi_ai::utils::uuid::now_ms() as u64;
    if now < not_before {
        if !drive.wait_for_retry {
            return Ok(ProcedureResult::Waiting {
                outcome: crate::harness::agent_harness::DriveOutcome::WaitingRetry {
                    operation_id: drive.operation_id.clone(),
                    not_before,
                },
            });
        }
        let _ = drive
            .gate
            .admit(|| wait_until(not_before, &drive.gate.signal))
            .map_err(|_| GateError::Closed("aborted".to_string()))?
            .await;
    }
    let published = lane
        .continue_operation(
            retry,
            Box::new(move |_state, current, _meta, _reader| {
                Box::pin(async move {
                    let next = OperationState::SummaryReady {
                        scope: current.scope().clone(),
                        task,
                        summary_context,
                        next_attempt,
                    };
                    OperationCommand::Commit {
                        writes: Vec::new(),
                        operation_state: next,
                        lane: None,
                        materialize: Box::new(|_| ProcedureResult::Continue),
                        events: None,
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;
    Ok(match published {
        ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
        ContinueOperationResult::Result { value } => value,
    })
}

/// 对应 `recoverStructuralGeneration`。
pub async fn recover_structural_generation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    effect: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let (summary_context, attempt) = match effect {
        OperationState::SummaryEffectPending {
            summary_context,
            attempt,
            ..
        } => (summary_context.clone(), *attempt),
        _ => unreachable!(),
    };
    let error = operation_error(
        "structural_interrupted",
        "Structural summary attempt was interrupted and its external outcome is unknown",
        None,
    );
    if attempt >= summary_context.retry_policy.max_attempts {
        return publish_structural_outcome(lane, drive, effect, StructuralOutcome::Failed { error }).await;
    }
    let retry_wait = retry_wait_from_effect(effect, &error.message);
    let published = lane
        .continue_operation(
            effect,
            Box::new(move |_state, _current, _meta, _reader| {
                Box::pin(async move {
                    OperationCommand::Commit {
                        writes: Vec::new(),
                        operation_state: retry_wait,
                        lane: None,
                        materialize: Box::new(|_| ProcedureResult::Continue),
                        events: None,
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;
    Ok(match published {
        ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
        ContinueOperationResult::Result { value } => value,
    })
}

// 使用边界类型以消除 unused import 警告。
#[allow(dead_code)]
fn _use_boundary() {
    let _ = |_e: &BoundaryFinishPending, _c: &crate::harness::session::types::CommitResult| ();
    let _ = |_l: &LaneRuntimeState, _c: &Continuation| ();
}
