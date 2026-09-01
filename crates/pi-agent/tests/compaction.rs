//! compaction 复刻的行为测试：重试分类 + prepareCompaction 边界。

use pi_agent::harness::compaction::compaction::{DEFAULT_COMPACTION_SETTINGS, prepare_compaction};
use pi_agent::harness::session::types::{CompactionEntry, Entry, EntryBase};
use pi_ai::{StopReason, faux_assistant_message, is_retryable_assistant_error};

#[test]
fn retry_classifies_transient_and_non_retryable_errors() {
    let mut message = faux_assistant_message(vec![], StopReason::Error);

    message.error_message = Some("rate limit exceeded, retry later".to_string());
    assert!(is_retryable_assistant_error(&message));

    message.error_message = Some("connection refused".to_string());
    assert!(is_retryable_assistant_error(&message));

    message.error_message = Some("insufficient_quota".to_string());
    assert!(!is_retryable_assistant_error(&message));

    message.error_message = Some("some deterministic failure".to_string());
    assert!(!is_retryable_assistant_error(&message));

    message.stop_reason = StopReason::Stop;
    assert!(!is_retryable_assistant_error(&message));
}

#[test]
fn prepare_compaction_empty_returns_none() {
    let result = prepare_compaction(&[], &DEFAULT_COMPACTION_SETTINGS).unwrap();
    assert!(result.is_none());
}

#[test]
fn prepare_compaction_skips_when_last_entry_is_compaction() {
    let compaction = Entry::Compaction(CompactionEntry {
        base: EntryBase {
            kind: "compaction".to_string(),
            id: "c1".to_string(),
            seq: 1,
            parent_id: None,
            timestamp: 1,
        },
        summary: "summary".to_string(),
        retained_tail: Vec::new(),
        tokens_before: 0,
        details: None,
        usage: None,
    });

    let result = prepare_compaction(&[compaction], &DEFAULT_COMPACTION_SETTINGS).unwrap();
    assert!(result.is_none());
}
