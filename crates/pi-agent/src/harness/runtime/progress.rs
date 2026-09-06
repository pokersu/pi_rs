//! Rust 翻译自 packages/agent/src/harness/runtime/progress.ts
//!
//! 进程本地进度通道：帧/工具输出写入 durable session 列表。

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::future::BoxFuture;
use pi_ai::utils::assistant_message_frame::AssistantMessageFrame;

use crate::harness::context::Context;
use crate::harness::runtime::types::{Lane, LaneCommand, LaneRuntimeState};
use crate::harness::session::types::{OperationState, SessionReader, ToolCall, Write};
use crate::harness::session::values::{
    ListCursor, ListOrder, ListReadOptions, append_list, pending_assistant_frames,
    pending_tool_output, set_value,
};
use crate::types::AgentToolResult;

/// 对应 `ProgressChannel<T>`。
pub trait ProgressChannel<T>: Send + Sync {
    fn write(&self, item: T);
    fn seal(&self);
    fn drain(&self) -> BoxFuture<'static, ()>;
}

struct OpenProgress<T, L: Lane> {
    sealed: Arc<AtomicBool>,
    latest: Arc<StdMutex<Option<BoxFuture<'static, ()>>>>,
    lane: Arc<L>,
    context: Context,
    commit_write: Arc<dyn Fn(T) -> Write + Send + Sync>,
    still_owns: Arc<dyn Fn(&LaneRuntimeState) -> bool + Send + Sync>,
    _marker: PhantomData<fn(T)>,
}

impl<T: Send + 'static, L: Lane + 'static> ProgressChannel<T> for OpenProgress<T, L> {
    fn write(&self, item: T) {
        if self.sealed.load(Ordering::SeqCst) {
            return;
        }
        let lane = Arc::clone(&self.lane);
        let context = self.context.clone();
        let commit_write = Arc::clone(&self.commit_write);
        let still_owns = Arc::clone(&self.still_owns);
        let future: BoxFuture<'static, ()> = Box::pin(async move {
            let write = commit_write(item);
            let _ = (*lane)
                .command(
                    Box::new(move |projection, _reader| {
                        Box::pin(async move {
                            if !still_owns(&projection) {
                                LaneCommand::Return { result: () }
                            } else {
                                LaneCommand::Commit {
                                    writes: vec![write],
                                    next: projection,
                                    materialize: Box::new(|_| ()),
                                    events: None,
                                }
                            }
                        })
                    }),
                    &context,
                )
                .await;
        });
        *self.latest.lock().unwrap() = Some(future);
    }

    fn seal(&self) {
        self.sealed.store(true, Ordering::SeqCst);
    }

    fn drain(&self) -> BoxFuture<'static, ()> {
        let future = self.latest.lock().unwrap().take();
        match future {
            Some(future) => future,
            None => Box::pin(async {}),
        }
    }
}

/// 对应 `openProgress`。
fn open_progress<T: Send + 'static, L: Lane + 'static>(
    lane: Arc<L>,
    context: Context,
    commit_write: Arc<dyn Fn(T) -> Write + Send + Sync>,
    still_owns: Arc<dyn Fn(&LaneRuntimeState) -> bool + Send + Sync>,
) -> OpenProgress<T, L> {
    OpenProgress {
        sealed: Arc::new(AtomicBool::new(false)),
        latest: Arc::new(StdMutex::new(None)),
        lane,
        context,
        commit_write,
        still_owns,
        _marker: PhantomData,
    }
}

/// 对应 `readAssistantFrames`：分页读取一个 response 的已提交 frame 前缀。
pub async fn read_assistant_frames(
    reader: &dyn SessionReader,
    operation_id: &str,
    response_entry_id: &str,
    context: &Context,
) -> Result<Vec<AssistantMessageFrame>, String> {
    let mut frames: Vec<AssistantMessageFrame> = Vec::new();
    let mut cursor: Option<ListCursor> = None;
    loop {
        let page = reader
            .read_list(
                &pending_assistant_frames(operation_id, response_entry_id).erased(),
                Some(ListReadOptions {
                    order: Some(ListOrder::Asc),
                    limit: Some(1_000),
                    cursor,
                }),
                context,
            )
            .await?;
        let page_len = page.len();
        for element in page {
            let frame: AssistantMessageFrame =
                serde_json::from_value(element.value).map_err(|e| e.to_string())?;
            frames.push(frame);
        }
        if page_len < 1_000 {
            return Ok(frames);
        }
        cursor = Some(ListCursor {
            seq: frames.len() as u64,
        });
    }
}

/// 对应 `openFrameProgress`。
pub fn open_frame_progress<L: Lane + 'static>(
    lane: Arc<L>,
    context: Context,
    operation_id: String,
    response_entry_id: String,
) -> impl ProgressChannel<AssistantMessageFrame> {
    let address = pending_assistant_frames(&operation_id, &response_entry_id);
    let still_response = response_entry_id.clone();
    open_progress(
        lane,
        context,
        Arc::new(move |frame| {
            Write::List(append_list(
                &address,
                serde_json::to_value(&frame).unwrap_or(serde_json::Value::Null),
            ))
        }),
        Arc::new(move |state| {
            let Some(operation) = &state.operation else {
                return false;
            };
            match &operation.state {
                OperationState::AssistantEffectPending {
                    response_entry_id, ..
                }
                | OperationState::DeferredEffectPending {
                    response_entry_id, ..
                } => *response_entry_id == still_response,
                _ => false,
            }
        }),
    )
}

/// 对应 `openToolProgress`。
pub fn open_tool_progress<L: Lane + 'static>(
    lane: Arc<L>,
    context: Context,
    operation_id: String,
    turn_id: String,
    source_index: usize,
    invocation_id: String,
) -> impl ProgressChannel<AgentToolResult> {
    let address = pending_tool_output(&operation_id, &invocation_id);
    let still_turn = turn_id.clone();
    let still_invocation = invocation_id.clone();
    open_progress(
        lane,
        context,
        Arc::new(move |snapshot| {
            Write::Value(set_value(
                &address,
                serde_json::to_value(&snapshot).unwrap_or(serde_json::Value::Null),
            ))
        }),
        Arc::new(move |state| {
            let Some(operation) = &state.operation else {
                return false;
            };
            let OperationState::Tools { batch, .. } = &operation.state else {
                return false;
            };
            batch.turn_id == still_turn
                && batch.calls.iter().any(|call| match call {
                    ToolCall::EffectPending {
                        source_index: si,
                        result_entry_id,
                        ..
                    } => *si == source_index && *result_entry_id == still_invocation,
                    _ => false,
                })
        }),
    )
}
