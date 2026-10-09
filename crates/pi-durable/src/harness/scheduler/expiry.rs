//! 对应 `scheduler.ts` 的上下文保留与过期：`#contextRetentionMs` / `#settleIdle` / `#scheduleExpiry`。
//!
//! 一个会话经任务运行时读到的上下文区间会被保留（`#contexts`，见 [`SchedulerState`]）：它只是缓存，
//! 丢弃是安全的。空闲超过 `settings.contextRetentionMs` 之后丢弃；重新有工作时清掉空闲起点。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `#scheduleExpiry()` 直接 `setTimeout` 并调用 `((timer as any).unref)()`。Rust 侧这里只产出
//!   [`ExpiryPlan`]——「未变 / 取消 / 在某时刻排一个 `after_ms` 的定时器」——由执行层交给宿主的定时器。
//!   这样这段决策可以脱离 async runtime 单测；`#settleIdle()` 的递归（定时器到期后再结算）也因此
//!   保持在执行层。
//! - 上游 `#contextRetentionMs()` 用 `try`/`catch` 把宿主取设置的异常挡在提交监听器之外，失败按 0 处理。
//!   Rust 的设置访问器是普通值（`Fn() -> Settings`），正常实现不会失败；这里不做 `catch_unwind`
//!   （它覆盖不了 `panic = "abort"`），改由宿主保证不 panic。`context_retention_ms` 因此只是取值。
//! - 上游 `#idleWaiters.keys()` 是 Map 的插入序；Rust 的 `Waiters` 用 `BTreeMap`，`keys()` 返回键序。
//!   每个等待键独立判定，顺序无语义差别。

use crate::harness::scheduler::state::SchedulerState;
use crate::harness::types::Settings;
use crate::harness::util::Waiters;
use crate::types::ConversationId;

/// 对应 `MAX_TIMER_DELAY`：`setTimeout` 支持的最长延时；更长的睡眠分几段。
pub const MAX_TIMER_DELAY: u64 = 2_147_483_647;

/// 对应 `#idleWaiters` 的键：`None` 等整个 Harness 空闲。
pub type IdleKey = Option<ConversationId>;

/// 对应 `#scheduleExpiry()` 的决策结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryPlan {
    /// 已排定的时间点没变，什么都不做。
    Unchanged,
    /// 取消现有定时器（若有）且不再排新的：没有空闲上下文，或正在关闭。
    Cancel,
    /// 取消现有定时器（若有），并在 `after_ms` 之后排一个到期回调。
    Schedule {
        /// 到期时刻（Harness 时间），即最早的 `idleSince + retention`。
        at: u64,
        /// 距现在多久触发，已按 `0` 与 [`MAX_TIMER_DELAY`] 夹取。
        after_ms: u64,
    },
}

/// 对应 `#contextRetentionMs()`：`settings.contextRetentionMs`。
///
/// 上游在此吞掉宿主取设置的异常并按 0 处理（保留的上下文只是缓存）；Rust 侧见模块文档的差异清单。
pub fn context_retention_ms(settings: &Settings) -> u64 {
    settings.context_retention_ms
}

/// 对应 `#settleIdle()`：结算空闲等待者，丢弃空闲过久的保留上下文，并给出定时器计划。
///
/// `current_expiry` 是执行层当前已排定的到期时刻（`None` 表示没有定时器）。
pub fn settle_idle(
    state: &mut SchedulerState,
    now: u64,
    retention: u64,
    idle_waiters: &Waiters<IdleKey, ()>,
    current_expiry: Option<u64>,
) -> ExpiryPlan {
    for conversation_id in idle_waiters.keys() {
        if state.idle(conversation_id) {
            idle_waiters.resolve(&conversation_id, ());
        }
    }

    let mut to_drop: Vec<ConversationId> = Vec::new();
    let mut to_clear: Vec<ConversationId> = Vec::new();
    let mut to_stamp: Vec<ConversationId> = Vec::new();
    for (conversation_id, kept) in &state.contexts {
        if kept
            .idle_since
            .is_some_and(|since| now.saturating_sub(since) >= retention)
        {
            to_drop.push(*conversation_id);
        } else if !state.idle(Some(*conversation_id)) {
            to_clear.push(*conversation_id);
        } else if retention > 0 {
            if kept.idle_since.is_none() {
                to_stamp.push(*conversation_id);
            }
        } else {
            // 保留时长为 0：空闲即丢弃。
            to_drop.push(*conversation_id);
        }
    }
    for conversation_id in to_drop {
        state.contexts.remove(&conversation_id);
    }
    for conversation_id in to_clear {
        if let Some(kept) = state.contexts.get_mut(&conversation_id) {
            kept.idle_since = None;
        }
    }
    for conversation_id in to_stamp {
        if let Some(kept) = state.contexts.get_mut(&conversation_id) {
            kept.idle_since = Some(now);
        }
    }

    schedule_expiry(state, now, retention, current_expiry)
}

/// 对应 `#scheduleExpiry()`：算出最早的保留上下文过期时刻，并给出该对它做什么。
pub fn schedule_expiry(
    state: &SchedulerState,
    now: u64,
    retention: u64,
    current_expiry: Option<u64>,
) -> ExpiryPlan {
    let mut at: Option<u64> = None;
    for kept in state.contexts.values() {
        if let Some(idle_since) = kept.idle_since {
            let candidate = idle_since + retention;
            if at.is_none_or(|earliest| candidate < earliest) {
                at = Some(candidate);
            }
        }
    }
    if current_expiry.is_some() && current_expiry == at {
        return ExpiryPlan::Unchanged;
    }
    let Some(at) = at else {
        return ExpiryPlan::Cancel;
    };
    if state.closing {
        return ExpiryPlan::Cancel;
    }
    ExpiryPlan::Schedule {
        at,
        after_ms: at.saturating_sub(now).min(MAX_TIMER_DELAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::context::EmptyContext;
    use crate::harness::context::{ContextBounds, ContextRange};
    use crate::harness::scheduler::ownership::AnyTaskRecord;
    use crate::harness::scheduler::state::KeptContext;
    use crate::harness::types::ContextView;
    use crate::types::{EntryId, TaskId, TaskRecord, TaskState};
    use serde_json::Value as JsonValue;
    use std::sync::Arc;

    fn context() -> Arc<dyn crate::chord::context::Context> {
        Arc::new(EmptyContext::new("[test]"))
    }

    fn empty_range() -> ContextRange {
        ContextRange {
            bounds: ContextBounds {
                head: None,
                tail: EntryId::new(0),
            },
            entries: Vec::new(),
            view: ContextView {
                head: None,
                entries: Vec::new(),
                contributions: Vec::new(),
                messages: Vec::new(),
            },
            edited: Default::default(),
            settled: Vec::new(),
            open: Vec::new(),
        }
    }

    fn kept(idle_since: Option<u64>) -> KeptContext {
        KeptContext {
            range: empty_range(),
            idle_since,
        }
    }

    /// 一个活动任务：它让所在会话不空闲。
    fn running_record(id: u64, conversation_id: u64) -> AnyTaskRecord {
        TaskRecord {
            id: TaskId::new(id),
            conversation_id: ConversationId::new(conversation_id),
            kind: "demo".to_string(),
            version: 1,
            input: JsonValue::Null,
            owner: None,
            background: false,
            abort_requested: false,
            started_at: None,
            ended_at: None,
            state: TaskState::Running {
                checkpoint: JsonValue::Null,
            },
            memos: None,
        }
    }

    #[test]
    fn settle_idle_stamps_an_idle_context_once_and_clears_it_when_work_resumes() {
        let mut state = SchedulerState::new();
        state.contexts.insert(ConversationId::new(1), kept(None));

        let plan = settle_idle(&mut state, 1_000, 60_000, &Waiters::new(), None);
        assert_eq!(
            state.contexts[&ConversationId::new(1)].idle_since,
            Some(1_000),
            "首次空闲时盖章"
        );
        assert_eq!(
            plan,
            ExpiryPlan::Schedule {
                at: 61_000,
                after_ms: 60_000
            }
        );

        // 再结算一次：已经盖过章，不再改动时刻。
        let plan = settle_idle(&mut state, 1_500, 60_000, &Waiters::new(), Some(61_000));
        assert_eq!(
            state.contexts[&ConversationId::new(1)].idle_since,
            Some(1_000)
        );
        assert_eq!(plan, ExpiryPlan::Unchanged, "最早到期时刻没变");
    }

    #[test]
    fn settle_idle_drops_a_context_idle_past_the_retention() {
        let mut state = SchedulerState::new();
        state
            .contexts
            .insert(ConversationId::new(1), kept(Some(1_000)));

        let plan = settle_idle(&mut state, 61_000, 60_000, &Waiters::new(), Some(61_000));
        assert!(state.contexts.is_empty(), "空闲已满保留时长");
        assert_eq!(plan, ExpiryPlan::Cancel, "没有保留上下文就没有定时器");
    }

    #[test]
    fn settle_idle_drops_contexts_immediately_when_retention_is_zero() {
        let mut state = SchedulerState::new();
        state.contexts.insert(ConversationId::new(1), kept(None));

        let plan = settle_idle(&mut state, 1_000, 0, &Waiters::new(), None);
        assert!(state.contexts.is_empty(), "保留时长为 0：空闲即丢弃");
        assert_eq!(plan, ExpiryPlan::Cancel);
    }

    #[tokio::test]
    async fn settle_idle_resolves_only_the_idle_waiters() {
        let mut state = SchedulerState::new();
        state.set_edge(ConversationId::new(1), None);
        state.set_edge(ConversationId::new(2), None);
        // 会话 2 里还有活动任务，因此不空闲。
        state.live.insert(TaskId::new(1), running_record(1, 2));

        let waiters: Arc<Waiters<IdleKey, ()>> = Arc::new(Waiters::new());
        let idle = waiters.add(Some(ConversationId::new(1)), context());
        let _busy = waiters.add(Some(ConversationId::new(2)), context());
        let _whole = waiters.add(None, context());

        settle_idle(&mut state, 1_000, 60_000, &waiters, None);

        assert!(idle.await.is_ok(), "空闲会话的等待被结清");
        assert!(waiters.keys().contains(&Some(ConversationId::new(2))));
        assert!(waiters.keys().contains(&None), "整个 Harness 不空闲");
    }

    #[test]
    fn schedule_expiry_clamps_a_late_expiry_to_the_maximum_delay() {
        let mut state = SchedulerState::new();
        state.contexts.insert(ConversationId::new(1), kept(Some(0)));

        let plan = schedule_expiry(&state, 0, u64::MAX / 2, None);
        assert_eq!(
            plan,
            ExpiryPlan::Schedule {
                at: u64::MAX / 2,
                after_ms: MAX_TIMER_DELAY
            },
            "超长睡眠分几段"
        );
    }

    #[test]
    fn schedule_expiry_cancels_while_closing() {
        let mut state = SchedulerState::new();
        state
            .contexts
            .insert(ConversationId::new(1), kept(Some(1_000)));
        state.closing = true;

        assert_eq!(
            schedule_expiry(&state, 2_000, 60_000, None),
            ExpiryPlan::Cancel
        );
    }

    #[test]
    fn schedule_expiry_picks_the_earliest_of_several_contexts() {
        let mut state = SchedulerState::new();
        state
            .contexts
            .insert(ConversationId::new(1), kept(Some(5_000)));
        state
            .contexts
            .insert(ConversationId::new(2), kept(Some(3_000)));
        state.contexts.insert(ConversationId::new(3), kept(None));

        assert_eq!(
            schedule_expiry(&state, 0, 1_000, None),
            ExpiryPlan::Schedule {
                at: 4_000,
                after_ms: 4_000
            },
            "未盖章的上下文不参与到期计算"
        );
    }
}
