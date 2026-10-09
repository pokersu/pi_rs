//! 对应 `harness/provider.ts`：会话的 provider 侧稳定身份。
//!
//! 上游的 `ensureProviderSessionId(runtime, context)` 接收 `TaskRuntime`（任务运行期接口，属 scheduler
//! 阶段），因此随 P5f/P5g 一并落地；本文件先落地文档定义与状态类型。

use std::sync::Arc;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::harness::types::TaskRuntime;
use crate::session::SessionError;
use crate::types::{
    ConversationFork, ConversationHistory, DocAccess, DocDefinitionSpec, DocToken,
    DocumentSemantics, JsonObject,
};

/// 对应 `ProviderState`：一个会话面向 provider 的稳定身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    /// Provider 侧会话 ID。
    pub session_id: String,
}

struct ProviderDefinition;

impl DocDefinitionSpec for ProviderDefinition {
    fn kind(&self) -> &str {
        "pi.provider"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Conversation {
            history: ConversationHistory::Latest,
            fork: ConversationFork::Initial,
        }
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        let state = ProviderState {
            session_id: pi_ai::uuidv7(),
        };
        match serde_json::to_value(state).expect("provider state serialises") {
            JsonValue::Object(object) => object,
            _ => JsonObject::new(),
        }
    }

    fn checkpoint_when(
        &self,
        _value: &JsonObject,
        _ops: &[crate::chord::delta::Op],
        _info: &crate::types::CheckpointInfo,
    ) -> bool {
        // 上游 `checkpointWhen: () => true`。
        true
    }
}

/// 对应 `ProviderDoc`：内置 provider 状态文档。
///
/// 每个分叉从全新的身份开始，而不是复制父会话的身份。
pub static PROVIDER_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    crate::documents::define_doc(Arc::new(ProviderDefinition)).expect("pi.provider")
});

/// 对应 `ensureProviderSessionId(runtime, context)`：返回已持久化的身份；正常路径不写。
///
/// 没有 `pi.provider` 的旧会话会在 provider 请求开始前得到一次迁移提交，其 `tx.doc()` 先跑 `initial()`。
pub async fn ensure_provider_session_id(
    runtime: &Arc<dyn TaskRuntime>,
    context: Arc<dyn Context>,
) -> Result<String, SessionError> {
    let conversation_id = runtime.conversation_id();
    let existing = runtime
        .snapshot(
            &*PROVIDER_DOC,
            Some(conversation_id.get()),
            None,
            Arc::clone(&context),
        )
        .await?;
    if let Some(existing) = existing
        && let Some(session_id) = existing.get("sessionId").and_then(JsonValue::as_str)
    {
        return Ok(session_id.to_string());
    }
    let created = Arc::new(std::sync::Mutex::new(None));
    let created_for_closure = Arc::clone(&created);
    runtime
        .commit(
            Box::new(move |tx, _current| {
                let created = Arc::clone(&created_for_closure);
                Box::pin(async move {
                    let draft = tx
                        .doc(
                            &*PROVIDER_DOC,
                            DocAccess {
                                owner: Some(conversation_id.get()),
                                key: None,
                            },
                            None,
                        )
                        .await
                        .map_err(SessionError::Doc)?;
                    let session_id = draft
                        .value()
                        .get("sessionId")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string();
                    *created.lock().expect("created") = Some(session_id);
                    Ok(None)
                })
            }),
            context,
        )
        .await?;
    match created.lock().expect("created").take() {
        Some(session_id) => Ok(session_id),
        None => Err(SessionError::Message(format!(
            "Conversation {conversation_id} has no provider session ID"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_kind_and_semantics_match_upstream() {
        let definition = PROVIDER_DOC.definition();
        assert_eq!(definition.kind(), "pi.provider");
        assert_eq!(definition.version(), 1);
        assert_eq!(
            definition.semantics(),
            DocumentSemantics::Conversation {
                history: ConversationHistory::Latest,
                fork: ConversationFork::Initial,
            }
        );
    }

    #[test]
    fn initial_state_carries_a_fresh_session_id() {
        let definition = PROVIDER_DOC.definition();
        let first = definition.initial(None);
        let second = definition.initial(None);
        assert!(first.contains_key("sessionId"));
        assert!(first["sessionId"].is_string());
        assert_ne!(
            first["sessionId"], second["sessionId"],
            "每次 initial() 都必须给新身份"
        );
    }

    #[test]
    fn every_change_checkpoints() {
        let definition = PROVIDER_DOC.definition();
        assert!(definition.checkpoint_when(
            &JsonObject::new(),
            &[],
            &crate::types::CheckpointInfo {
                deltas_since_base: 5,
            }
        ));
    }

    #[test]
    fn state_round_trips_as_camel_case() {
        let state = ProviderState {
            session_id: "0190f".to_string(),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json, serde_json::json!({"sessionId": "0190f"}));
        assert_eq!(
            serde_json::from_value::<ProviderState>(json).unwrap(),
            state
        );
    }
}
