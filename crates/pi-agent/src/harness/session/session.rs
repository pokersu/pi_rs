//! Rust 翻译自 packages/agent/src/harness/session/session.ts
//!
//! StorageBackedSession：基于 Storage 的 Session 实现 + Mutation + Branch。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use pi_ai::uuidv7;
use serde_json::Value as Json;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::harness::context::Context;
use crate::harness::session::commit::insert_entry;
use crate::harness::session::types::{
    self, Branch, BranchScan, CommitResult, Entry, EntryQuery, EntryScan, IdGenerator, NewEntry,
    ScanOrder, Session, SessionMetadata, SessionMutation, SessionReader, SessionStats, Storage,
    StorageBranchScan, Write,
};
use crate::harness::session::values::{
    ListElement, ListReadOptions, StoredValue, Value, ValueList, append_list as append_list_write,
    branch_tip, delete_list as delete_list_write, delete_value as delete_value_write, entry_label,
    session_name, set_value as set_value_write,
};
use crate::types::AgentMessage;

struct UuidV7IdGenerator;

impl IdGenerator for UuidV7IdGenerator {
    fn next(&self, timestamp_ms: Option<u64>) -> String {
        match timestamp_ms {
            None => uuidv7(),
            Some(ts) => pi_ai::utils::uuid::uuidv7_with_timestamp(ts),
        }
    }
}

fn pending_assistant_check(writes: &[Write]) -> Result<(), String> {
    for write in writes {
        if let Write::Entry(entry_write) = write
            && let NewEntry::Message(message) = &entry_write.entry
            && let AgentMessage::Assistant(a) = &message.message
            && a.stop_reason == pi_ai::StopReason::Pending
        {
            return Err("Cannot persist a pending assistant message".to_string());
        }
    }
    Ok(())
}

struct StorageBackedSessionMutation {
    storage: Arc<dyn Storage>,
    release: StdMutex<Option<OwnedMutexGuard<()>>>,
    active: AtomicBool,
    committed: AtomicBool,
}

#[async_trait::async_trait]
impl SessionReader for StorageBackedSessionMutation {
    async fn get_entries(
        &self,
        ids: &[String],
        context: &Context,
    ) -> Result<std::collections::BTreeMap<String, Entry>, String> {
        self.assert_active()?;
        self.storage.get_entries(ids, context).await
    }
    async fn get_stats(&self, context: &Context) -> Result<SessionStats, String> {
        self.assert_active()?;
        self.storage.get_stats(context).await
    }
    async fn get_value(
        &self,
        address: &Value<Json>,
        context: &Context,
    ) -> Result<Option<StoredValue<Json>>, String> {
        self.assert_active()?;
        self.storage.get_value(address, context).await
    }
    async fn scan_values(
        &self,
        prefix: &Value<Json>,
        context: &Context,
    ) -> Result<Vec<StoredValue<Json>>, String> {
        self.assert_active()?;
        self.storage.scan_values(prefix, context).await
    }
    async fn read_list(
        &self,
        address: &ValueList<Json>,
        options: Option<ListReadOptions>,
        context: &Context,
    ) -> Result<Vec<ListElement<Json>>, String> {
        self.assert_active()?;
        self.storage.read_list(address, options, context).await
    }
    async fn scan_branch(
        &self,
        query: StorageBranchScan,
        context: &Context,
    ) -> Result<Vec<Entry>, String> {
        self.assert_active()?;
        self.storage.scan_branch(query, context).await
    }
}

#[async_trait::async_trait]
impl SessionMutation for StorageBackedSessionMutation {
    async fn commit(&self, writes: Vec<Write>, context: &Context) -> Result<CommitResult, String> {
        self.assert_active()?;
        if self.committed.swap(true, Ordering::SeqCst) {
            return Err("SessionMutator commit already attempted".to_string());
        }
        pending_assistant_check(&writes)?;
        self.storage.commit(writes, context).await
    }

    async fn end(&self, _context: &Context) -> Result<(), String> {
        self.active.store(false, Ordering::SeqCst);
        if let Some(release) = self.release.lock().unwrap().take() {
            drop(release);
        }
        Ok(())
    }
}

impl StorageBackedSessionMutation {
    fn assert_active(&self) -> Result<(), String> {
        if !self.active.load(Ordering::SeqCst) {
            return Err("SessionMutator cannot be used outside its mutation callback".to_string());
        }
        Ok(())
    }
}

struct StorageBackedBranch {
    name: String,
    session: Arc<StorageBackedSession>,
}

#[async_trait::async_trait]
impl Branch for StorageBackedBranch {
    fn name(&self) -> &str {
        &self.name
    }
    async fn get_tip_id(&self, context: &Context) -> Result<Option<String>, String> {
        self.session.get_branch_tip(&self.name, context).await
    }
    async fn find_entries(
        &self,
        query: Option<BranchScan>,
        context: &Context,
    ) -> Result<Vec<Entry>, String> {
        let query = query.unwrap_or_default();
        let start = match query.start.clone() {
            Some(s) => s,
            None => match self.get_tip_id(context).await? {
                None => return Ok(Vec::new()),
                Some(tip) => tip,
            },
        };
        let scan = StorageBranchScan {
            scan: BranchScan {
                order: Some(query.order.unwrap_or(ScanOrder::NewestFirst)),
                ..query
            },
            start,
        };
        self.session.scan_branch(scan, context).await
    }
    async fn find_entry(
        &self,
        query: Option<BranchScan>,
        context: &Context,
    ) -> Result<Option<Entry>, String> {
        let mut query = query.unwrap_or_default();
        query.limit = Some(query.limit.map(|l| l.min(1)).unwrap_or(1));
        Ok(self
            .find_entries(Some(query), context)
            .await?
            .into_iter()
            .next())
    }
    async fn append_message(
        &self,
        message: AgentMessage,
        context: &Context,
    ) -> Result<String, String> {
        self.session
            .append_to_branch(&self.name, NewBranchEntry::Message(message), context)
            .await
    }
    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Json>,
        context: &Context,
    ) -> Result<String, String> {
        self.session
            .append_to_branch(
                &self.name,
                NewBranchEntry::Custom { custom_type, data },
                context,
            )
            .await
    }
}

#[allow(clippy::large_enum_variant)]
enum NewBranchEntry {
    Message(AgentMessage),
    Custom {
        custom_type: String,
        data: Option<Json>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Open,
    Closing,
    Closed,
}

/// 对应 `StorageBackedSession`。
pub struct StorageBackedSession {
    metadata: SessionMetadata,
    id_generator: Arc<dyn IdGenerator>,
    storage: Arc<dyn Storage>,
    mutation_lock: Arc<Mutex<()>>,
    on_close: Option<Arc<dyn Fn() + Send + Sync>>,
    branches: StdMutex<std::collections::HashMap<String, Arc<StorageBackedBranch>>>,
    state: StdMutex<SessionState>,
    close_promise: StdMutex<Option<()>>,
}

#[async_trait::async_trait]
impl SessionReader for StorageBackedSession {
    async fn get_entries(
        &self,
        ids: &[String],
        context: &Context,
    ) -> Result<std::collections::BTreeMap<String, Entry>, String> {
        self.assert_open()?;
        self.storage.get_entries(ids, context).await
    }
    async fn get_stats(&self, context: &Context) -> Result<SessionStats, String> {
        self.assert_open()?;
        self.storage.get_stats(context).await
    }
    async fn get_value(
        &self,
        address: &Value<Json>,
        context: &Context,
    ) -> Result<Option<StoredValue<Json>>, String> {
        self.assert_open()?;
        self.storage.get_value(address, context).await
    }
    async fn scan_values(
        &self,
        prefix: &Value<Json>,
        context: &Context,
    ) -> Result<Vec<StoredValue<Json>>, String> {
        self.assert_open()?;
        self.storage.scan_values(prefix, context).await
    }
    async fn read_list(
        &self,
        address: &ValueList<Json>,
        options: Option<ListReadOptions>,
        context: &Context,
    ) -> Result<Vec<ListElement<Json>>, String> {
        self.assert_open()?;
        self.storage.read_list(address, options, context).await
    }
    async fn scan_branch(
        &self,
        query: StorageBranchScan,
        context: &Context,
    ) -> Result<Vec<Entry>, String> {
        self.assert_open()?;
        self.storage.scan_branch(query, context).await
    }
}

#[async_trait::async_trait]
impl Session for StorageBackedSession {
    fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }
    fn id_generator(&self) -> Arc<dyn IdGenerator> {
        Arc::clone(&self.id_generator)
    }
    async fn get_entry(&self, id: &str, context: &Context) -> Result<Option<Entry>, String> {
        Ok(self
            .get_entries(&[id.to_string()], context)
            .await?
            .remove(id))
    }
    async fn get_name(&self, context: &Context) -> Result<Option<String>, String> {
        Ok(self
            .get_value(&session_name(), context)
            .await?
            .map(|s| s.value)
            .and_then(|v| v.as_str().map(String::from)))
    }
    async fn get_label(
        &self,
        target_id: &str,
        context: &Context,
    ) -> Result<Option<String>, String> {
        Ok(self
            .get_value(&entry_label(target_id), context)
            .await?
            .map(|s| s.value)
            .and_then(|v| v.as_str().map(String::from)))
    }
    async fn find_entries(
        &self,
        query: Option<EntryQuery>,
        context: &Context,
    ) -> Result<Vec<Entry>, String> {
        self.assert_open()?;
        let query = query.unwrap_or_default();
        let order = query.order.unwrap_or(ScanOrder::Desc);
        let mut scan = EntryScan {
            entry_type: query.entry_type,
            custom_type: query.custom_type,
            order: Some(order),
            limit: query.limit,
            from_seq: None,
            to_seq: None,
        };
        if let Some(cursor) = query.cursor {
            match order {
                ScanOrder::Asc => scan.from_seq = Some(cursor.seq + 1),
                _ => scan.to_seq = Some(cursor.seq.saturating_sub(1)),
            }
        }
        self.storage.scan_entries(scan, context).await
    }
    async fn find_entry(
        &self,
        query: Option<EntryQuery>,
        context: &Context,
    ) -> Result<Option<Entry>, String> {
        let mut query = query.unwrap_or_default();
        query.limit = Some(query.limit.map(|l| l.min(1)).unwrap_or(1));
        Ok(self
            .find_entries(Some(query), context)
            .await?
            .into_iter()
            .next())
    }
    async fn branch(
        &self,
        name: &str,
        context: &Context,
    ) -> Result<Option<Arc<dyn Branch>>, String> {
        Self::assert_valid_branch_name(name)?;
        if self.get_value(&branch_tip(name), context).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(self.get_or_create_branch_object(name)))
    }
    async fn create_branch(
        &self,
        name: &str,
        at: Option<String>,
        context: &Context,
    ) -> Result<Arc<dyn Branch>, String> {
        self.assert_open()?;
        Self::assert_valid_branch_name(name)?;
        let name_owned = name.to_string();
        types::mutate(
            self,
            Arc::new(move |mutator, ctx| {
                let name = name_owned.clone();
                let at = at.clone();
                Box::pin(async move {
                    if mutator.get_value(&branch_tip(&name), &ctx).await?.is_some() {
                        return Err(format!("Branch already exists: {name}"));
                    }
                    if let Some(at) = &at
                        && !mutator
                            .get_entries(std::slice::from_ref(at), &ctx)
                            .await?
                            .contains_key(at)
                    {
                        return Err(format!("Unknown target: {at}"));
                    }
                    mutator
                        .commit(
                            vec![Write::Value(set_value_write(
                                &branch_tip(&name),
                                serde_json::to_value(at).unwrap_or(Json::Null),
                            ))],
                            &ctx,
                        )
                        .await?;
                    Ok(())
                })
            }),
            context,
        )
        .await?;
        Ok(self.get_or_create_branch_object(name))
    }
    async fn begin_mutation(&self, _context: &Context) -> Result<Arc<dyn SessionMutation>, String> {
        self.assert_open()?;
        let guard = Arc::clone(&self.mutation_lock).lock_owned().await;
        Ok(Arc::new(StorageBackedSessionMutation {
            storage: Arc::clone(&self.storage),
            release: StdMutex::new(Some(guard)),
            active: AtomicBool::new(true),
            committed: AtomicBool::new(false),
        }))
    }
    async fn set_value(
        &self,
        address: &Value<Json>,
        next: Json,
        context: &Context,
    ) -> Result<(), String> {
        let address = address.clone();
        types::mutate(
            self,
            Arc::new(move |mutator, ctx| {
                let address = address.clone();
                let next = next.clone();
                Box::pin(async move {
                    mutator
                        .commit(vec![Write::Value(set_value_write(&address, next))], &ctx)
                        .await?;
                    Ok(())
                })
            }),
            context,
        )
        .await
    }
    async fn delete_value(&self, address: &Value<Json>, context: &Context) -> Result<(), String> {
        let address = address.clone();
        types::mutate(
            self,
            Arc::new(move |mutator, ctx| {
                let address = address.clone();
                Box::pin(async move {
                    mutator
                        .commit(vec![Write::Value(delete_value_write(&address))], &ctx)
                        .await?;
                    Ok(())
                })
            }),
            context,
        )
        .await
    }
    async fn append_list(
        &self,
        address: &ValueList<Json>,
        element: Json,
        context: &Context,
    ) -> Result<(), String> {
        let address = address.clone();
        types::mutate(
            self,
            Arc::new(move |mutator, ctx| {
                let address = address.clone();
                let element = element.clone();
                Box::pin(async move {
                    mutator
                        .commit(
                            vec![Write::List(append_list_write(&address, element))],
                            &ctx,
                        )
                        .await?;
                    Ok(())
                })
            }),
            context,
        )
        .await
    }
    async fn delete_list(
        &self,
        address: &ValueList<Json>,
        context: &Context,
    ) -> Result<(), String> {
        let address = address.clone();
        types::mutate(
            self,
            Arc::new(move |mutator, ctx| {
                let address = address.clone();
                Box::pin(async move {
                    mutator
                        .commit(vec![Write::List(delete_list_write(&address))], &ctx)
                        .await?;
                    Ok(())
                })
            }),
            context,
        )
        .await
    }
    async fn set_name(&self, name: Option<String>, context: &Context) -> Result<(), String> {
        match name {
            None => self.delete_value(&session_name(), context).await,
            Some(n) => {
                self.set_value(&session_name(), Json::String(n), context)
                    .await
            }
        }
    }
    async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
        context: &Context,
    ) -> Result<(), String> {
        let address = entry_label(target_id);
        match label {
            None => self.delete_value(&address, context).await,
            Some(l) => self.set_value(&address, Json::String(l), context).await,
        }
    }
    async fn close(&self, context: &Context) -> Result<(), String> {
        if self.close_promise.lock().unwrap().is_some() {
            return Ok(());
        }
        *self.state.lock().unwrap() = SessionState::Closing;
        let _ = self.storage.close(context).await;
        *self.state.lock().unwrap() = SessionState::Closed;
        if let Some(on_close) = &self.on_close {
            on_close();
        }
        Ok(())
    }
}

impl Clone for StorageBackedSession {
    fn clone(&self) -> Self {
        Self {
            metadata: self.metadata.clone(),
            id_generator: Arc::clone(&self.id_generator),
            storage: Arc::clone(&self.storage),
            mutation_lock: Arc::clone(&self.mutation_lock),
            on_close: self.on_close.clone(),
            branches: StdMutex::new(std::collections::HashMap::new()),
            state: StdMutex::new(*self.state.lock().unwrap()),
            close_promise: StdMutex::new(None),
        }
    }
}

impl StorageBackedSession {
    pub fn new(
        metadata: SessionMetadata,
        storage: Arc<dyn Storage>,
        on_close: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            metadata,
            id_generator: Arc::new(UuidV7IdGenerator),
            storage,
            mutation_lock: Arc::new(Mutex::new(())),
            on_close,
            branches: StdMutex::new(std::collections::HashMap::new()),
            state: StdMutex::new(SessionState::Open),
            close_promise: StdMutex::new(None),
        }
    }

    async fn get_branch_tip(
        &self,
        name: &str,
        context: &Context,
    ) -> Result<Option<String>, String> {
        match self.get_value(&branch_tip(name), context).await? {
            None => Err(format!("Unknown branch: {name}")),
            Some(stored) => Ok(serde_json::from_value(stored.value).unwrap_or(None)),
        }
    }

    async fn append_to_branch(
        &self,
        name: &str,
        entry: NewBranchEntry,
        context: &Context,
    ) -> Result<String, String> {
        self.assert_open()?;
        if let NewBranchEntry::Message(AgentMessage::Assistant(a)) = &entry
            && a.stop_reason == pi_ai::StopReason::Pending
        {
            return Err("Cannot persist a pending assistant message".to_string());
        }
        let id = self.id_generator.next(None);
        let id_for_closure = id.clone();
        let name = name.to_string();
        types::mutate(
            self,
            Arc::new(move |mutator, ctx| {
                let name = name.clone();
                let id = id_for_closure.clone();
                let entry = entry.clone();
                Box::pin(async move {
                    let tip = mutator.get_value(&branch_tip(&name), &ctx).await?;
                    let tip = tip.ok_or_else(|| format!("Unknown branch: {name}"))?;
                    let parent_id: Option<String> =
                        serde_json::from_value(tip.value).unwrap_or(None);
                    let new_entry = match entry {
                        NewBranchEntry::Message(message) => {
                            NewEntry::Message(crate::harness::session::types::NewMessageEntry {
                                id: id.clone(),
                                parent_id,
                                custom_type: None,
                                message,
                                terminate: None,
                            })
                        }
                        NewBranchEntry::Custom { custom_type, data } => {
                            NewEntry::Custom(crate::harness::session::types::NewCustomEntry {
                                id: id.clone(),
                                parent_id,
                                custom_type,
                                data,
                            })
                        }
                    };
                    mutator
                        .commit(
                            vec![
                                insert_entry(new_entry),
                                Write::Value(set_value_write(
                                    &branch_tip(&name),
                                    serde_json::to_value(&id).unwrap_or(Json::Null),
                                )),
                            ],
                            &ctx,
                        )
                        .await?;
                    Ok(())
                })
            }),
            context,
        )
        .await?;
        Ok(id)
    }

    fn get_or_create_branch_object(&self, name: &str) -> Arc<StorageBackedBranch> {
        let mut branches = self.branches.lock().unwrap();
        if let Some(branch) = branches.get(name) {
            return Arc::clone(branch);
        }
        let branch = Arc::new(StorageBackedBranch {
            name: name.to_string(),
            session: Arc::new(self.clone()),
        });
        branches.insert(name.to_string(), Arc::clone(&branch));
        branch
    }

    fn assert_valid_branch_name(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("branch name must not be empty".to_string());
        }
        if name.contains('\0') {
            return Err("branch name must not contain \\u0000".to_string());
        }
        Ok(())
    }

    fn assert_open(&self) -> Result<(), String> {
        if *self.state.lock().unwrap() != SessionState::Open {
            return Err("Session is closed".to_string());
        }
        Ok(())
    }
}

impl Clone for NewBranchEntry {
    fn clone(&self) -> Self {
        match self {
            NewBranchEntry::Message(m) => NewBranchEntry::Message(m.clone()),
            NewBranchEntry::Custom { custom_type, data } => NewBranchEntry::Custom {
                custom_type: custom_type.clone(),
                data: data.clone(),
            },
        }
    }
}
