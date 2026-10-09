//! 对应 `harness/registry.ts`：注册表的写面与内置任务。
//!
//! 注册表把「已安装扩展 + 内置任务」发布成不可变的 [`RegistrySnapshot`]（[`RegistryReader`] 的只读面），
//! 并在每次发布时通知监听者。内置任务不是扩展，不可移除或替换。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `install` / `#publish` 抛错（校验失败或任务名冲突）；Rust 的 `install` 是同步的，
//!   用 panic 表达对等语义（与 `Storage::mint_id` 一致）。
//! - 上游 `subscribe` 返回 `() => void`；Rust 用 `Box<dyn Fn() + Send + Sync>`，按 `Arc` 指针识别
//!   待移除的监听者。
//! - 三个内置任务相互依赖（generation 的 `start_run` 需要 generation 任务本身），用 `OnceLock` 打破循环。

use std::sync::{Arc, Mutex, OnceLock};

use crate::harness::agent::INSTRUCTIONS_KEY;
use crate::harness::compaction::{make_compaction_task, make_create_compaction};
use crate::harness::generation::{make_generation_task, make_start_run};
use crate::harness::tool::make_tool_task;
use crate::harness::types::{Extension, RegistryReader, RegistrySnapshot};
use crate::types::Task;

/// 对应 `SECTION_KEY`：合法的章节键。
fn section_key_valid(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

/// 对应 `validateExtension(extension)`：单个扩展内工具名与章节键唯一，键合法且未占用。
fn validate_extension(extension: &Extension) {
    let mut tools = std::collections::BTreeSet::new();
    for tool in &extension.tools {
        if !tools.insert(tool.name().to_string()) {
            panic!(
                "Extension {} has two tools named {}",
                extension.name,
                tool.name()
            );
        }
    }
    let mut sections = std::collections::BTreeSet::new();
    for section in &extension.sections {
        let key = section.key();
        if !section_key_valid(key) {
            panic!("Section key {key:?} must match ^[a-z][a-z0-9_-]*$");
        }
        if key == INSTRUCTIONS_KEY {
            panic!("Section key {key} is reserved for the agent's instructions");
        }
        if !sections.insert(key.to_string()) {
            panic!(
                "Extension {} has two sections with key {key}",
                extension.name
            );
        }
    }
}

/// 对应 `Registry`：注册表的写面。
pub trait Registry: RegistryReader {
    /// 对应 `install(extension)`：安装或替换同名扩展。
    fn install(&self, extension: Arc<Extension>);

    /// 对应 `uninstall(extension)`：按名卸载。
    fn uninstall(&self, extension: &Extension);
}

/// 对应 `RegistryImpl`。
struct RegistryImpl {
    /// 内置任务（不可移除或替换）。
    builtin: Vec<Arc<Task>>,
    /// 已安装扩展（按安装顺序）。
    extensions: Mutex<Vec<Arc<Extension>>>,
    /// 发布监听者。
    listeners: Arc<Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>>,
}

impl RegistryImpl {
    /// 由已安装扩展构造不可变快照；任务名冲突时 panic（对应上游 `#publish` 的 throw）。
    fn build_snapshot(&self, extensions: Vec<Arc<Extension>>) -> RegistrySnapshot {
        let mut tasks = self.builtin.clone();
        for extension in &extensions {
            for task in &extension.tasks {
                let name = task.definition().name();
                if tasks
                    .iter()
                    .any(|installed| installed.definition().name() == name)
                {
                    panic!(
                        "Task {name} of extension {} is already installed",
                        extension.name
                    );
                }
                tasks.push(Arc::clone(task));
            }
        }
        RegistrySnapshot::new(extensions, tasks)
    }

    /// 通知监听者。
    fn notify(&self) {
        for listener in self.listeners.lock().expect("listeners").iter() {
            listener();
        }
    }
}

impl RegistryReader for RegistryImpl {
    fn snapshot(&self) -> RegistrySnapshot {
        let extensions = self.extensions.lock().expect("extensions").clone();
        self.build_snapshot(extensions)
    }

    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn Fn() + Send + Sync> {
        let listeners = Arc::clone(&self.listeners);
        let listener_ptr = Arc::as_ptr(&listener) as *const () as usize;
        self.listeners
            .lock()
            .expect("listeners")
            .push(Arc::clone(&listener));
        Box::new(move || {
            listeners
                .lock()
                .expect("listeners")
                .retain(|installed| Arc::as_ptr(installed) as *const () as usize != listener_ptr);
        })
    }
}

impl Registry for RegistryImpl {
    fn install(&self, extension: Arc<Extension>) {
        validate_extension(&extension);
        let mut extensions = self.extensions.lock().expect("extensions");
        match extensions
            .iter()
            .position(|installed| installed.name == extension.name)
        {
            Some(index) => extensions[index] = extension,
            None => extensions.push(extension),
        }
        let snapshot = self.build_snapshot(extensions.clone());
        drop(extensions);
        // 构造即校验：快照构造成功后再通知。
        drop(snapshot);
        self.notify();
    }

    fn uninstall(&self, extension: &Extension) {
        let mut extensions = self.extensions.lock().expect("extensions");
        if !extensions
            .iter()
            .any(|installed| installed.name == extension.name)
        {
            return;
        }
        extensions.retain(|installed| installed.name != extension.name);
        drop(extensions);
        self.notify();
    }
}

/// 对应 `createRegistry()`：创建一个只含内置任务的应用自有注册表。
pub fn create_registry() -> Arc<dyn Registry> {
    // 三个内置任务相互依赖（generation 的 start_run 需要 generation 任务本身），用 OnceLock 打破循环。
    let generation_cell: Arc<OnceLock<Arc<Task>>> = Arc::new(OnceLock::new());
    let start_run = make_start_run(Arc::new({
        let cell = Arc::clone(&generation_cell);
        move || cell.get().expect("generation task").clone()
    }));
    let compaction_task = make_compaction_task(Arc::clone(&start_run));
    let create_compaction = make_create_compaction(Arc::clone(&compaction_task));
    let tool_task = make_tool_task();
    let generation_task = make_generation_task(
        Arc::clone(&start_run),
        create_compaction,
        Arc::clone(&tool_task),
        Arc::new({
            let cell = Arc::clone(&generation_cell);
            move || cell.get().expect("generation task").clone()
        }),
    );
    generation_cell.set(Arc::clone(&generation_task)).ok();

    let builtin = vec![generation_task, tool_task, compaction_task];
    Arc::new(RegistryImpl {
        builtin,
        extensions: Mutex::new(Vec::new()),
        listeners: Arc::new(Mutex::new(Vec::new())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::context::Context;
    use crate::harness::types::{PromptInput, PromptSection};

    struct Section {
        key: &'static str,
    }

    #[async_trait::async_trait]
    impl PromptSection for Section {
        fn key(&self) -> &str {
            self.key
        }

        async fn render(
            &self,
            _input: &PromptInput,
            _context: Arc<dyn Context>,
        ) -> Result<Option<String>, crate::session::SessionError> {
            Ok(Some(format!("section {}", self.key)))
        }
    }

    #[test]
    fn builtin_tasks_are_installed_in_the_snapshot() {
        let registry = create_registry();
        let snapshot = registry.snapshot();
        let names: Vec<&str> = snapshot
            .tasks()
            .iter()
            .map(|task| task.definition().name())
            .collect();
        assert_eq!(names, vec!["pi.generation", "pi.tool", "pi.compaction"]);
        assert!(snapshot.installed().is_empty());
    }

    #[test]
    fn section_key_validation_matches_upstream() {
        assert!(section_key_valid("abc"));
        assert!(section_key_valid("a-b_c9"));
        assert!(!section_key_valid("A"));
        assert!(!section_key_valid("1a"));
        assert!(!section_key_valid(""));
        assert!(!section_key_valid("a b"));
    }

    #[test]
    #[should_panic(expected = "reserved for the agent's instructions")]
    fn validate_extension_rejects_reserved_instructions_key() {
        let extension = Extension {
            name: "demo".to_string(),
            tools: Vec::new(),
            sections: vec![Arc::new(Section {
                key: INSTRUCTIONS_KEY,
            })],
            hooks: Vec::new(),
            wraps: Vec::new(),
            tasks: Vec::new(),
        };
        validate_extension(&extension);
    }
}
