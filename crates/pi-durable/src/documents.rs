//! 对应 `src/documents.ts`：文档定义的构造、地址解析与物化校验。
//!
//! # 与上游的差异
//!
//! 上游 `resolveAddress` 接受**重载参数列表**（`args: readonly unknown[]`），按定义的 `scope` 决定
//! 是否消费 owner 与 family key。Rust 没有 rest args，因此改为显式参数
//! （[`resolve_address`] 的 `owner` / `key`），语义不变。

use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::types::{
    ConversationFork, ConversationHistory, DocDefinitionSpec, DocFamilyToken, DocToken,
    DocumentAddress, DocumentCreate, DocumentId, DocumentRecord, DocumentScope, DocumentSemantics,
    JsonObject, StoredDocument,
};

/// 对应 `validateDefinition` / `checkRecordScope` / `checkRecordVersion` 抛出的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocError {
    /// 定义版本必须是正整数。
    InvalidVersion(String),
    /// 缺少或非法的 owner ID。
    MissingOwner(String),
    /// 记录的 scope/history/fork 与定义不符。
    ScopeMismatch(String),
    /// 记录版本比定义更新。
    NewerVersion(String),
    /// 缺少所需迁移。
    NeedsMigration(String),
    /// 对应上游文档层抛出的其他 `Error` / `TypeError`。
    Message(String),
}

impl std::fmt::Display for DocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocError::InvalidVersion(message)
            | DocError::MissingOwner(message)
            | DocError::ScopeMismatch(message)
            | DocError::NewerVersion(message)
            | DocError::NeedsMigration(message)
            | DocError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DocError {}

/// 对应 `defineDoc`：定义单例文档并返回其 token。
pub fn define_doc(definition: Arc<dyn DocDefinitionSpec>) -> Result<DocToken, DocError> {
    validate_definition(definition.as_ref())?;
    Ok(DocToken::new(definition))
}

/// 对应 `defineDocFamily`：定义键控文档家族并返回其 token。
pub fn define_doc_family(
    definition: Arc<dyn DocDefinitionSpec>,
) -> Result<DocFamilyToken, DocError> {
    validate_definition(definition.as_ref())?;
    Ok(DocFamilyToken::new(definition))
}

/// 对应 `validateDefinition`。
fn validate_definition(definition: &dyn DocDefinitionSpec) -> Result<(), DocError> {
    if definition.version() < 1 {
        return Err(DocError::InvalidVersion(format!(
            "Document {} version must be a positive integer",
            definition.kind()
        )));
    }
    Ok(())
}

/// 对应 `ResolvedAddress`：逻辑地址 + 其字符串身份 + 下一个参数位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAddress {
    /// 逻辑地址。
    pub address: DocumentAddress,
    /// 稳定字符串身份。
    pub id: String,
    /// 对应 `nextArgument`：消费掉的参数个数（0、1 或 2）。
    pub next_argument: usize,
}

/// 对应 `resolveAddress`。
pub fn resolve_address(
    definition: &dyn DocDefinitionSpec,
    owner: Option<u64>,
    key: Option<String>,
) -> Result<ResolvedAddress, DocError> {
    let mut next_argument = 0usize;

    let scope = match definition.semantics() {
        DocumentSemantics::Session => DocumentScope::Session,
        DocumentSemantics::Conversation { .. } => {
            let conversation_id =
                crate::types::ConversationId::new(require_owner(definition, owner)?);
            next_argument += 1;
            DocumentScope::Conversation { conversation_id }
        }
        DocumentSemantics::Task => {
            let task_id = crate::types::TaskId::new(require_owner(definition, owner)?);
            next_argument += 1;
            DocumentScope::Task { task_id }
        }
    };

    let key = if definition.is_family() {
        next_argument += 1;
        key
    } else {
        None
    };

    let address = DocumentAddress {
        kind: definition.kind().to_string(),
        scope,
        key,
    };
    Ok(ResolvedAddress {
        id: address_id(&address),
        address,
        next_argument,
    })
}

/// 对应 `ownerId`：取出必填的 owner ID。
///
/// TS 还要检查 `typeof value === "number" && Number.isSafeInteger(value)`；Rust 的 `Option<u64>`
/// 已在类型上保证，因此只需处理缺省。
fn require_owner(definition: &dyn DocDefinitionSpec, value: Option<u64>) -> Result<u64, DocError> {
    value.ok_or_else(|| {
        let scope = match definition.semantics() {
            DocumentSemantics::Session => "session",
            DocumentSemantics::Conversation { .. } => "conversation",
            DocumentSemantics::Task => "task",
        };
        DocError::MissingOwner(format!(
            "Document {} requires a {scope} ID",
            definition.kind()
        ))
    })
}

/// 对应 `addressId`：一个逻辑地址的稳定字符串身份。
pub fn address_id(address: &DocumentAddress) -> String {
    let owner = match &address.scope {
        DocumentScope::Session => JsonValue::Null,
        DocumentScope::Conversation {
            conversation_id, ..
        } => JsonValue::from(conversation_id.get()),
        DocumentScope::Task { task_id } => JsonValue::from(task_id.get()),
    };
    let scope_kind = match &address.scope {
        DocumentScope::Session => "session",
        DocumentScope::Conversation { .. } => "conversation",
        DocumentScope::Task { .. } => "task",
    };
    let key = address
        .key
        .as_ref()
        .map_or(JsonValue::Null, |key| JsonValue::String(key.clone()));

    serde_json::to_string(&JsonValue::Array(vec![
        JsonValue::String(address.kind.clone()),
        JsonValue::String(scope_kind.to_string()),
        owner,
        key,
    ]))
    .expect("address id is JSON-serialisable")
}

/// 对应 `HasDocumentScope`：让校验函数同时接受 `DocumentCreate` 与 `DocumentRecord`。
pub trait HasDocumentScope {
    /// 记录 ID。
    fn record_id(&self) -> DocumentId;
    /// 记录 kind。
    fn record_kind(&self) -> &str;
    /// 记录 scope（不含 history/fork）。
    fn record_scope(&self) -> &DocumentScope;
    /// 记录顶层的 `history`（只有会话文档声明）。
    fn record_history(&self) -> Option<ConversationHistory>;
    /// 记录顶层的 `fork`（只有会话文档声明）。
    fn record_fork(&self) -> Option<ConversationFork>;
}

impl HasDocumentScope for DocumentCreate {
    fn record_id(&self) -> DocumentId {
        self.id
    }
    fn record_kind(&self) -> &str {
        &self.kind
    }
    fn record_scope(&self) -> &DocumentScope {
        &self.scope
    }
    fn record_history(&self) -> Option<ConversationHistory> {
        self.history
    }
    fn record_fork(&self) -> Option<ConversationFork> {
        self.fork
    }
}

impl HasDocumentScope for DocumentRecord {
    fn record_id(&self) -> DocumentId {
        self.id
    }
    fn record_kind(&self) -> &str {
        &self.kind
    }
    fn record_scope(&self) -> &DocumentScope {
        &self.scope
    }
    fn record_history(&self) -> Option<ConversationHistory> {
        self.history
    }
    fn record_fork(&self) -> Option<ConversationFork> {
        self.fork
    }
}

/// 对应 `AnyDocToken`：擦除单例/家族区别的令牌视图（`doc(token, ...)` 接收两种令牌）。
pub trait AnyDocToken: Send + Sync {
    /// 令牌携带的定义。
    fn definition(&self) -> &Arc<dyn DocDefinitionSpec>;
}

impl AnyDocToken for DocToken {
    fn definition(&self) -> &Arc<dyn DocDefinitionSpec> {
        DocToken::definition(self)
    }
}

impl AnyDocToken for DocFamilyToken {
    fn definition(&self) -> &Arc<dyn DocDefinitionSpec> {
        DocFamilyToken::definition(self)
    }
}

/// 对应 `documentCreate`：为一个地址上的新化身构造存储创建记录。
pub fn document_create(
    definition: &dyn DocDefinitionSpec,
    address: &DocumentAddress,
    id: DocumentId,
) -> DocumentCreate {
    let (history, fork) = match definition.semantics() {
        DocumentSemantics::Conversation { history, fork } => (Some(history), Some(fork)),
        _ => (None, None),
    };
    DocumentCreate {
        id,
        kind: address.kind.clone(),
        key: address.key.clone(),
        history,
        fork,
        scope: address.scope,
    }
}

/// 对应 `checkRecordScope`：拒绝与定义语义不符的访问。
pub fn check_record_scope(
    definition: &dyn DocDefinitionSpec,
    record: &dyn HasDocumentScope,
) -> Result<(), DocError> {
    let expected = definition.semantics();
    let actual = *record.record_scope();
    let matches = match (expected, actual) {
        (DocumentSemantics::Session, DocumentScope::Session) => true,
        (DocumentSemantics::Task, DocumentScope::Task { .. }) => true,
        (DocumentSemantics::Conversation { history, fork }, DocumentScope::Conversation { .. }) => {
            record.record_history() == Some(history) && record.record_fork() == Some(fork)
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(DocError::ScopeMismatch(format!(
            "Document {} ({}) does not match the supplied definition semantics",
            record.record_id(),
            record.record_kind()
        )))
    }
}

/// 对应 `checkRecordVersion`：拒绝无法使用的存储版本。
pub fn check_record_version(
    definition: &dyn DocDefinitionSpec,
    record: &dyn HasDocumentScope,
    version: u32,
) -> Result<(), DocError> {
    if version > definition.version() {
        return Err(DocError::NewerVersion(format!(
            "Document {} ({}) has newer version {version} than {}",
            record.record_id(),
            record.record_kind(),
            definition.version()
        )));
    }
    if version < definition.version() && !definition.has_migrate() {
        return Err(DocError::NeedsMigration(format!(
            "Document {} ({}) requires migration from version {version}",
            record.record_id(),
            record.record_kind()
        )));
    }
    Ok(())
}

/// 对应 `materializeDocument`。
pub fn materialize_document(
    definition: &dyn DocDefinitionSpec,
    stored: &StoredDocument,
) -> Result<JsonObject, DocError> {
    materialize_document_value(
        definition,
        &stored.record,
        stored.version,
        stored.value.clone(),
    )
}

/// 对应 `materializeDocumentValue`：校验并按需迁移一个脱离存储的值。
pub fn materialize_document_value(
    definition: &dyn DocDefinitionSpec,
    record: &dyn HasDocumentScope,
    version: u32,
    value: JsonObject,
) -> Result<JsonObject, DocError> {
    check_record_scope(definition, record)?;
    check_record_version(definition, record, version)?;
    if version == definition.version() {
        return Ok(value);
    }
    definition.migrate(&value, version).ok_or_else(|| {
        DocError::NeedsMigration(format!(
            "Document {} ({}) requires migration from version {version}",
            record.record_id(),
            record.record_kind()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ConversationFork, ConversationHistory, ConversationId, DocumentId, JsonObject, TaskId,
    };
    use serde_json::json;

    struct SessionDoc;

    impl DocDefinitionSpec for SessionDoc {
        fn kind(&self) -> &str {
            "demo.session"
        }
        fn version(&self) -> u32 {
            1
        }
        fn semantics(&self) -> DocumentSemantics {
            DocumentSemantics::Session
        }
        fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
            JsonObject::new()
        }
    }

    struct ConversationFamily;

    impl DocDefinitionSpec for ConversationFamily {
        fn kind(&self) -> &str {
            "demo.conversation"
        }
        fn version(&self) -> u32 {
            1
        }
        fn semantics(&self) -> DocumentSemantics {
            DocumentSemantics::Conversation {
                history: ConversationHistory::Latest,
                fork: ConversationFork::Current,
            }
        }
        fn is_family(&self) -> bool {
            true
        }
        fn initial(&self, seed: Option<&JsonValue>) -> JsonObject {
            let mut value = JsonObject::new();
            if let Some(seed) = seed {
                value.insert("seed".to_string(), seed.clone());
            }
            value
        }
    }

    struct TaskDoc;

    impl DocDefinitionSpec for TaskDoc {
        fn kind(&self) -> &str {
            "demo.task"
        }
        fn version(&self) -> u32 {
            2
        }
        fn semantics(&self) -> DocumentSemantics {
            DocumentSemantics::Task
        }
        fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
            JsonObject::new()
        }
    }

    #[test]
    fn address_id_matches_upstream_tuple_shape() {
        let address = DocumentAddress {
            kind: "demo.conversation".to_string(),
            scope: DocumentScope::Conversation {
                conversation_id: ConversationId::new(7),
            },
            key: Some("alpha".to_string()),
        };
        assert_eq!(
            address_id(&address),
            r#"["demo.conversation","conversation",7,"alpha"]"#,
        );

        let session = DocumentAddress {
            kind: "demo.session".to_string(),
            scope: DocumentScope::Session,
            key: None,
        };
        assert_eq!(
            address_id(&session),
            r#"["demo.session","session",null,null]"#
        );
    }

    #[test]
    fn resolve_address_consumes_owner_for_conversation() {
        let resolved =
            resolve_address(&ConversationFamily, Some(3), Some("k".to_string())).unwrap();
        assert_eq!(resolved.next_argument, 2, "owner + family key");
        match resolved.address.scope {
            DocumentScope::Conversation { conversation_id } => {
                assert_eq!(conversation_id.get(), 3)
            }
            other => panic!("expected conversation scope, got {other:?}"),
        }
        assert_eq!(resolved.address.key.as_deref(), Some("k"));
    }

    #[test]
    fn resolve_address_session_needs_no_owner_and_ignores_key() {
        let resolved = resolve_address(&SessionDoc, None, Some("ignored".to_string())).unwrap();
        assert_eq!(resolved.next_argument, 0);
        assert_eq!(resolved.address.scope, DocumentScope::Session);
        assert_eq!(resolved.address.key, None, "非家族定义不消费 key");
    }

    #[test]
    fn resolve_address_requires_owner_for_scoped_docs() {
        let error = resolve_address(&TaskDoc, None, None).unwrap_err();
        assert_eq!(error.to_string(), "Document demo.task requires a task ID",);
    }

    #[test]
    fn validate_definition_rejects_zero_version() {
        struct Bad;
        impl DocDefinitionSpec for Bad {
            fn kind(&self) -> &str {
                "demo.bad"
            }
            fn version(&self) -> u32 {
                0
            }
            fn semantics(&self) -> DocumentSemantics {
                DocumentSemantics::Session
            }
            fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
                JsonObject::new()
            }
        }

        let error = validate_definition(&Bad).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Document demo.bad version must be a positive integer",
        );
    }

    #[test]
    fn check_record_scope_rejects_semantics_mismatch() {
        let record = DocumentCreate {
            id: DocumentId::new(1),
            kind: "demo.session".to_string(),
            key: None,
            history: None,
            fork: None,
            scope: DocumentScope::Session,
        };
        // SessionDoc 与记录匹配。
        assert!(check_record_scope(&SessionDoc, &record).is_ok());
        // TaskDoc 与记录不匹配。
        let error = check_record_scope(&TaskDoc, &record).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Document 1 (demo.session) does not match the supplied definition semantics",
        );
    }

    #[test]
    fn check_record_version_requires_migration_for_older_version() {
        let record = DocumentCreate {
            id: DocumentId::new(2),
            kind: "demo.task".to_string(),
            key: None,
            history: None,
            fork: None,
            scope: DocumentScope::Task {
                task_id: TaskId::new(0),
            },
        };
        // TaskDoc 版本为 2，记录为 1 且无迁移。
        let error = check_record_version(&TaskDoc, &record, 1).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Document 2 (demo.task) requires migration from version 1",
        );

        // 记录版本更新。
        let error = check_record_version(&TaskDoc, &record, 5).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Document 2 (demo.task) has newer version 5 than 2",
        );
    }

    #[test]
    fn materialize_document_value_passes_through_current_version() {
        let record = DocumentCreate {
            id: DocumentId::new(3),
            kind: "demo.session".to_string(),
            key: None,
            history: None,
            fork: None,
            scope: DocumentScope::Session,
        };
        let mut value = JsonObject::new();
        value.insert("n".to_string(), json!(1));

        let materialized =
            materialize_document_value(&SessionDoc, &record, 1, value.clone()).unwrap();
        assert_eq!(materialized, value);
    }
}
