//! 对应 `env/node-watch.ts`：基于快照对比的文件监视器。
//!
//! 与上游相同的设计：native 事件只触发一次去抖后的重扫，真正的变化来自「快照差集 + 事件路径」，
//! 因此被替换的文件、被重命名/重建的祖先目录、连同内容一起新建的目录，都不依赖操作系统实际发送了
//! 哪些事件。新目录会在下一次扫描前先装上 watcher，所以 watcher 建立之前写入的内容不会被遗漏。
//!
//! 语言机制映射（已在对应处注释）：
//! - `fs.watch` → `notify`（`RecommendedWatcher`），`Installed.watcher` 退化为 `dev`/`ino` 身份，
//!   关闭单个监听用 `Watcher::unwatch`。
//! - `setTimeout` / `clearTimeout` → `tokio::time::sleep` + 代次计数（见 `schedule_settle`）。
//! - `Map` / `Set` → `HashMap` / `HashSet`。
//! - `#running` 的 Promise 去重 → `running` 标志 + `flush_lock`（`tokio::sync::Mutex`）。
//! - `#sync` / `#scan` 的异步文件系统操作 → `tokio::fs`。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::chord::context::Context;
use crate::env::{FileError, FileErrorCode, FileWatcher, WatchChange, WatchMode, WatchTarget};

// ─── 常量 ────────────────────────────────────────────────────────────────────

/// 对应 `DEBOUNCE_MS`。
const DEBOUNCE_MS: u64 = 50;
/// 对应 `FSEVENTS_SETTLE_MS`。macOS 上 `fs.watch`（`notify` 的 FSEvents 后端）返回早于事件流可用，
/// 安装 watcher 后再等这么久重扫一次可捕获期间的变化。
const FSEVENTS_SETTLE_MS: u64 = 500;
/// 对应 `DEFAULT_POLL_MS`。
const DEFAULT_POLL_MS: u64 = 2000;
/// 对应 `DEFAULT_MAX_DIRECTORIES`。
const DEFAULT_MAX_DIRECTORIES: usize = 10_000;
/// 对应 `HASH_MAX_BYTES`：轮询模式下最近修改的小文件额外按内容比较。
const HASH_MAX_BYTES: u64 = 256 * 1024;
/// 对应 `HASH_RECENT_MS`。
const HASH_RECENT_MS: f64 = 5000.0;

/// 对应 `UNRELIABLE_FILE_SYSTEMS`：接受 watch 但不报告远端变化的文件系统的 Linux `statfs` magic。
#[rustfmt::skip]
#[allow(dead_code)] // 仅在 Linux / Android 上被 `any_unreliable` 读取。
const UNRELIABLE_FILE_SYSTEMS: &[u64] = &[
    0x6969,     // NFS
    0x517b,     // SMB
    0xff534d42, // CIFS
    0xfe534d42, // SMB2
    0x65735546, // FUSE（sshfs、Android 共享存储）
    0x01021997, // 9P（WSL2 Windows 驱动器）
    0x0bd00bd0, // Lustre
    0x47504653, // GPFS
    0x00c36400, // Ceph
    0x5346414f, // OpenAFS
    0x6b414653, // kAFS
    0x5dca2df5, // sdcardfs
];

// ─── 类型 ────────────────────────────────────────────────────────────────────

/// 对应 `NodeWatchOptions`：`NodeExecutionEnv` 如何监视。
#[derive(Debug, Clone, Default)]
pub struct NodeWatchOptions {
    /// 对应 `mode`：强制一种模式。默认 Windows 与不报告远端变化的文件系统上选择 `polling`。
    pub mode: Option<WatchMode>,
    /// 对应 `pollIntervalMs`：`polling` 模式下两次快照的间隔；默认 2000 ms。
    pub poll_interval_ms: Option<u64>,
    /// 对应 `maxDirectories`：一个 watcher 覆盖的最多目录数；默认 10,000。
    pub max_directories: Option<usize>,
}

/// 对应 `ResolvedTarget`。
#[derive(Clone)]
struct ResolvedTarget {
    path: String,
    recursive: bool,
    hidden: bool,
    names: HashSet<String>,
}

/// 对应 `Entry["kind"]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// 对应 `Entry`：快照里一个路径记得的内容；目录与祖先只按身份（dev/ino）比较。
#[derive(Clone)]
struct Entry {
    kind: EntryKind,
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ms: f64,
    hash: Option<String>,
}

/// 对应 `Snapshot`。
type Snapshot = HashMap<String, Entry>;

/// 对应 `Installed`：watcher 安装位置的身份——被另一个文件替换的路径需要新的 watcher。
struct Installed {
    dev: u64,
    ino: u64,
}

/// 对应 `Scan`：快照，以及指向文件的符号链接目标（它们的文件需要单独的 watcher）。
struct Scan {
    snapshot: Snapshot,
    linked_files: HashSet<String>,
}

/// 对应 `BudgetExceeded` 抛出的错误消息（直接在 [`FileError`] 中表达）。
fn budget_exceeded(max_directories: usize) -> FileError {
    FileError::new(
        FileErrorCode::Invalid,
        format!("Watched paths exceed {max_directories} directories"),
        None,
    )
}

// ─── 辅助函数 ────────────────────────────────────────────────────────────────

/// 对应 `isDenied`：`EACCES` / `EPERM`。
fn is_denied(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
}

/// 对应 `isNodeError` 且 `code` 命中给定集合。
fn is_node_error_kind(error: &std::io::Error, codes: &[i32]) -> bool {
    matches!(error.raw_os_error(), Some(code) if codes.contains(&code))
}

/// 对应 `toFileError`：把 `std::io::Error` 映射为 [`FileError`]。
fn to_file_error(error: std::io::Error, path: Option<&str>) -> FileError {
    let message = error.to_string();
    let code = match error.raw_os_error() {
        Some(libc::ENOENT) => FileErrorCode::NotFound,
        Some(libc::EACCES) | Some(libc::EPERM) => FileErrorCode::PermissionDenied,
        Some(libc::ENOTDIR) => FileErrorCode::NotDirectory,
        _ => FileErrorCode::Unknown,
    };
    FileError::new(code, message, path.map(str::to_string))
}

/// 对应 `metadata.dev`。
#[cfg(unix)]
fn metadata_dev(stats: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    stats.dev()
}
#[cfg(windows)]
fn metadata_dev(stats: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    stats.volume_serial_number().unwrap_or(0) as u64
}

/// 对应 `metadata.ino`。
#[cfg(unix)]
fn metadata_ino(stats: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    stats.ino()
}
#[cfg(windows)]
fn metadata_ino(stats: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    stats.file_index().unwrap_or(0)
}

/// 对应 `stats.mtimeMs`。
fn mtime_ms(stats: &std::fs::Metadata) -> f64 {
    stats
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}

/// 对应 `Date.now() - stats.mtimeMs < HASH_RECENT_MS`。
fn is_recent(stats: &std::fs::Metadata) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0);
    now - mtime_ms(stats) < HASH_RECENT_MS
}

/// 对应 `createHash("sha256").update(content).digest("hex")`。
fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(content))
}

/// 对应 `entryOf`。
fn entry_of(stats: &std::fs::Metadata, hash: Option<&str>) -> Entry {
    let kind = if stats.is_file() {
        EntryKind::File
    } else if stats.is_dir() {
        EntryKind::Directory
    } else if stats.file_type().is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    Entry {
        kind,
        dev: metadata_dev(stats),
        ino: metadata_ino(stats),
        size: if kind == EntryKind::Directory {
            0
        } else {
            stats.len()
        },
        mtime_ms: if kind == EntryKind::Directory {
            0.0
        } else {
            mtime_ms(stats)
        },
        hash: hash.map(str::to_string),
    }
}

/// 对应 `sameEntry`。
fn same_entry(a: &Entry, b: &Entry) -> bool {
    a.kind == b.kind
        && a.dev == b.dev
        && a.ino == b.ino
        && a.size == b.size
        && a.mtime_ms == b.mtime_ms
        && a.hash == b.hash
}

/// 对应 `dirname`。
fn dirname(path: &str) -> String {
    match Path::new(path).parent() {
        None => path.to_string(),
        Some(parent) => {
            let s = parent.to_string_lossy();
            if s.is_empty() {
                ".".to_string()
            } else {
                s.into_owned()
            }
        }
    }
}

/// 对应 `ancestorsOf`：`path` 的祖先，最近者优先，直到根。
fn ancestors_of(path: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = dirname(path);
    loop {
        let next = dirname(&current);
        let done = next == current;
        result.push(current);
        if done {
            return result;
        }
        current = next;
    }
}

/// 对应 `isWithin`：`path` 是否为 `ancestor` 或其下。
fn is_within(path: &str, ancestor: &str) -> bool {
    if path == ancestor {
        return true;
    }
    let sep = std::path::MAIN_SEPARATOR;
    let ancestor = if ancestor.ends_with(sep) {
        ancestor.to_string()
    } else {
        format!("{ancestor}{sep}")
    };
    path.starts_with(&ancestor)
}

/// 对应 `excluded`。
fn excluded(target: &ResolvedTarget, name: &str) -> bool {
    (target.hidden && name.starts_with('.')) || target.names.contains(name)
}

/// 对应 `directory + sep + name`。
fn join_path(directory: &str, name: &str) -> String {
    if directory.ends_with(std::path::MAIN_SEPARATOR) {
        format!("{directory}{name}")
    } else {
        format!("{directory}{}{name}", std::path::MAIN_SEPARATOR)
    }
}

/// 对应 `relative(ancestor, path).split(sep)`：`path` 相对 `ancestor` 的组件。
fn relative_components(ancestor: &str, path: &str) -> Vec<String> {
    match Path::new(path).strip_prefix(Path::new(ancestor)) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 对应 `reconcileWatchers` 里对 `fs.watch` 错误的分流：非 ENOENT/EACCES/EPERM 即转入轮询。
fn reconcile_should_poll(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::PathNotFound => false,
        notify::ErrorKind::Io(io) => !is_denied(io),
        _ => true,
    }
}

/// 对应 `statfs` 的 `f_type`（仅 Linux / Android 编译）。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn statfs_type(path: &str) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(Path::new(path).as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } == 0 {
        Some(buf.f_type as u64)
    } else {
        None
    }
}

/// 对应 `anyUnreliable`：是否有路径（或最近存在的祖先）位于不报告远端变化的文件系统上。
async fn any_unreliable(paths: &[String]) -> bool {
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = paths;
        false
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        for path in paths {
            for candidate in std::iter::once(path.clone()).chain(ancestors_of(path)) {
                match statfs_type(&candidate) {
                    None => continue,
                    Some(fs_type) => {
                        if UNRELIABLE_FILE_SYSTEMS.contains(&fs_type) {
                            return true;
                        }
                        break;
                    }
                }
            }
        }
        false
    }
}

// ─── 状态 ────────────────────────────────────────────────────────────────────

/// 对应 `NodeFileWatcher`。
pub struct NodeFileWatcher {
    inner: Arc<Inner>,
}

struct Inner {
    on_change: Box<dyn Fn(WatchChange) + Send + Sync>,
    state: Mutex<State>,
    /// `notify` 的单例 watcher；`drop` 即关闭所有监听。
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// 对应 `#running`：串行化 `#flush`，并让 `close` 等待正在运行的 flush。
    flush_lock: tokio::sync::Mutex<()>,
}

struct State {
    targets: Vec<ResolvedTarget>,
    /// 最近一次扫描得出的「指向文件的符号链接」目标，用于事件分流（对应 `Scan.linkedFiles`）。
    linked_files: HashSet<String>,
    watchers: HashMap<String, Installed>,
    events: HashSet<String>,
    snapshot: Snapshot,
    mode: WatchMode,
    poll_interval_ms: u64,
    max_directories: usize,
    /// 对应 `#running !== undefined`。
    running: bool,
    /// 对应 `#dirty`。
    dirty: bool,
    /// 对应 `#closed`。
    closed: bool,
    /// 对应 `#timer !== undefined`（去抖与轮询共用同一个 timer）。
    timer_present: bool,
    /// 对应 `#settleTimer` 的「重置」语义（`clearTimeout` + `setTimeout`）。
    settle_generation: u64,
}

/// notify 回调投递到事件循环的消息。
enum EventMsg {
    Path(String),
    Rescan,
}

impl NodeFileWatcher {
    /// 对应 `open`：先装 watcher，再建立后续变化与之比较的快照。
    pub async fn open(
        targets: &[WatchTarget],
        resolve_path: impl Fn(&str) -> String + Send + Sync + 'static,
        on_change: Box<dyn Fn(WatchChange) + Send + Sync>,
        options: NodeWatchOptions,
    ) -> Result<NodeFileWatcher, FileError> {
        let resolved: Vec<ResolvedTarget> = targets
            .iter()
            .map(|target| ResolvedTarget {
                path: resolve_path(&target.path),
                recursive: target.recursive == Some(true),
                hidden: target.exclude.as_ref().and_then(|e| e.hidden) == Some(true),
                names: target
                    .exclude
                    .as_ref()
                    .and_then(|e| e.names.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            })
            .collect();

        // Windows 拒绝重命名仍打开着下方目录的目录，native watcher 会让每个被监听的目录保持打开，
        // 因此 Windows 改为轮询；不报告远端变化的文件系统同样如此。
        let mode = match options.mode {
            Some(mode) => mode,
            None => {
                if cfg!(target_os = "windows")
                    || any_unreliable(&resolved.iter().map(|t| t.path.clone()).collect::<Vec<_>>())
                        .await
                {
                    WatchMode::Polling
                } else {
                    WatchMode::Native
                }
            }
        };

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventMsg>();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            match result {
                Ok(event) => {
                    for path in event.paths {
                        let _ = event_tx.send(EventMsg::Path(path.to_string_lossy().into_owned()));
                    }
                }
                // 对应 `watcher.on("error")`：重扫兜底。
                Err(_) => {
                    let _ = event_tx.send(EventMsg::Rescan);
                }
            }
        })
        .map_err(|error| {
            FileError::new(
                FileErrorCode::Invalid,
                format!("Cannot watch files: {error}"),
                None,
            )
        })?;

        let inner = Arc::new(Inner {
            on_change,
            state: Mutex::new(State {
                targets: resolved,
                linked_files: HashSet::new(),
                watchers: HashMap::new(),
                events: HashSet::new(),
                snapshot: HashMap::new(),
                mode,
                poll_interval_ms: options.poll_interval_ms.unwrap_or(DEFAULT_POLL_MS),
                max_directories: options.max_directories.unwrap_or(DEFAULT_MAX_DIRECTORIES),
                running: false,
                dirty: false,
                closed: false,
                timer_present: false,
                settle_generation: 0,
            }),
            watcher: Mutex::new(Some(watcher)),
            flush_lock: tokio::sync::Mutex::new(()),
        });

        // 事件循环：把 notify 的事件路径分流为 on_event / on_linked_file_event。
        let loop_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            while let Some(message) = event_rx.recv().await {
                handle_event_msg(&loop_inner, message);
            }
        });

        let watcher = NodeFileWatcher { inner };

        if let Err(error) = sync(&watcher.inner, false).await {
            stop(&watcher.inner);
            return Err(error);
        }
        schedule_poll(&watcher.inner);
        Ok(watcher)
    }

    /// 对应 `mode`。
    fn current_mode(&self) -> WatchMode {
        self.inner.state.lock().expect("state").mode
    }
}

#[async_trait::async_trait]
impl FileWatcher for NodeFileWatcher {
    fn mode(&self) -> WatchMode {
        self.current_mode()
    }

    /// 对应 `close`：停止监听，并等待正在运行的 flush 结束；幂等。
    async fn close(&self, _context: &dyn Context) {
        if self.inner.state.lock().expect("state").closed {
            return;
        }
        // 对应 `await this.#running`。
        let _guard = self.inner.flush_lock.lock().await;
        stop(&self.inner);
    }
}

// ─── 事件循环与调度 ─────────────────────────────────────────────────────────

fn handle_event_msg(inner: &Arc<Inner>, message: EventMsg) {
    match message {
        EventMsg::Path(path) => on_event(inner, path),
        EventMsg::Rescan => schedule_flush(inner),
    }
}

/// 对应 `#onEvent` 与 `#onLinkedFileEvent` 的分流。
fn on_event(inner: &Arc<Inner>, path: String) {
    let (linked, closed, is_watched_dir, targets) = {
        let state = inner.state.lock().expect("state");
        (
            state.linked_files.contains(&path),
            state.closed,
            state.watchers.contains_key(&path),
            state.targets.clone(),
        )
    };
    if closed {
        return;
    }
    if linked {
        on_linked_file_event(inner, path);
        return;
    }
    // 关于祖先无关兄弟、关于被排除条目的事件被忽略。
    let relevant = in_scope(&targets, &path);
    if relevant {
        let reported = reported(&path, &targets);
        inner.state.lock().expect("state").events.insert(reported);
    }
    // 对应 `#onEvent` 里 `relevant || filename === undefined`：目录级事件总是触发一次重扫。
    if relevant || is_watched_dir {
        schedule_flush(inner);
    }
}

/// 对应 `#onLinkedFileEvent`：目标链接的文件变化了，报告目标。
fn on_linked_file_event(inner: &Arc<Inner>, target: String) {
    if inner.state.lock().expect("state").closed {
        return;
    }
    inner.state.lock().expect("state").events.insert(target);
    schedule_flush(inner);
}

/// 对应 `#inScope`。
fn in_scope(targets: &[ResolvedTarget], path: &str) -> bool {
    for target in targets {
        if is_within(&target.path, path) {
            return true;
        }
        if !is_within(path, &target.path) || path == target.path {
            continue;
        }
        let components = relative_components(&target.path, path);
        if !target.recursive && components.len() > 1 {
            continue;
        }
        if components.iter().any(|name| excluded(target, name)) {
            continue;
        }
        return true;
    }
    false
}

/// 对应 `#reported`。
fn reported(path: &str, targets: &[ResolvedTarget]) -> String {
    for target in targets {
        if is_within(path, &target.path) {
            return path.to_string();
        }
    }
    for target in targets {
        if is_within(&target.path, path) {
            return target.path.clone();
        }
    }
    path.to_string()
}

/// 对应 `#deliver`：抛错的回调不应停止监视。
fn deliver(inner: &Arc<Inner>, change: WatchChange) {
    if inner.state.lock().expect("state").closed {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (inner.on_change)(change);
    }));
}

/// 对应 `#stop`：关闭并清理。
fn stop(inner: &Arc<Inner>) {
    {
        let mut state = inner.state.lock().expect("state");
        state.closed = true;
        state.timer_present = false;
        state.settle_generation += 1;
        state.watchers.clear();
    }
    inner.watcher.lock().expect("watcher").take();
}

/// 对应 `#scheduleFlush`：不重置已有的 timer。
fn schedule_flush(inner: &Arc<Inner>) {
    {
        let mut state = inner.state.lock().expect("state");
        if state.closed || state.timer_present {
            return;
        }
        state.timer_present = true;
    }
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;
        inner.state.lock().expect("state").timer_present = false;
        flush(&inner).await;
    });
}

/// 对应 `#schedulePoll`。
fn schedule_poll(inner: &Arc<Inner>) {
    let interval = {
        let mut state = inner.state.lock().expect("state");
        if state.closed || state.mode != WatchMode::Polling || state.timer_present {
            return;
        }
        state.timer_present = true;
        state.poll_interval_ms
    };
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
        inner.state.lock().expect("state").timer_present = false;
        flush(&inner).await;
        // 对应 `void this.#flush().finally(() => this.#schedulePoll())`。
        schedule_poll(&inner);
    });
}

/// 对应 `#scheduleSettle`（重置语义）。
fn schedule_settle(inner: &Arc<Inner>) {
    let generation = {
        let mut state = inner.state.lock().expect("state");
        if state.closed {
            return;
        }
        state.settle_generation += 1;
        state.settle_generation
    };
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(FSEVENTS_SETTLE_MS)).await;
        {
            let state = inner.state.lock().expect("state");
            if state.settle_generation != generation {
                return;
            }
        }
        flush(&inner).await;
    });
}

/// 对应 `#flush` 的去重语义：若已在运行则标记 dirty 并复用同一个 running。
async fn flush(inner: &Arc<Inner>) {
    loop {
        let start = {
            let mut state = inner.state.lock().expect("state");
            if state.running {
                state.dirty = true;
                false
            } else {
                state.running = true;
                true
            }
        };
        if start {
            // 串行执行 flush 主体；`close` 也通过这把锁等待它结束。
            let _guard = inner.flush_lock.lock().await;
            flush_loop(inner).await;
            inner.state.lock().expect("state").running = false;
            return;
        }
        // 已有 flush 在运行：等它结束，再按最新的 dirty 状态决定是否补一轮。
        let _guard = inner.flush_lock.lock().await;
    }
}

/// 对应 `#flush` 的 IIFE 主体。
async fn flush_loop(inner: &Arc<Inner>) {
    loop {
        inner.state.lock().expect("state").dirty = false;
        let events: Vec<String> = inner.state.lock().expect("state").events.drain().collect();
        let changed = match sync(inner, true).await {
            Ok(changed) => changed,
            Err(error) => {
                // 对应 #flush 的 catch：isDenied → permission_denied，其余 → invalid。
                let file_error = if error.code == FileErrorCode::PermissionDenied {
                    error
                } else {
                    FileError::new(FileErrorCode::Invalid, error.message, error.path)
                };
                deliver(inner, WatchChange::Error(file_error));
                stop(inner);
                return;
            }
        };
        let mut changed = changed;
        for path in events {
            changed.insert(path);
        }
        if !changed.is_empty() {
            let mut paths: Vec<String> = changed.into_iter().collect();
            paths.sort();
            deliver(inner, WatchChange::Paths(paths));
        }
        let (dirty, closed) = {
            let state = inner.state.lock().expect("state");
            (state.dirty, state.closed)
        };
        if !dirty || closed {
            break;
        }
    }
}

// ─── 扫描与 watcher 协调 ─────────────────────────────────────────────────────

/// 对应 `#sync`：重扫、报告差异，并为新目录安装 watcher，直到没有新目录为止。
async fn sync(inner: &Arc<Inner>, mut report: bool) -> Result<HashSet<String>, FileError> {
    let mut changed: HashSet<String> = HashSet::new();
    for _round in 0..10 {
        if inner.state.lock().expect("state").closed {
            break;
        }
        let scan = scan(inner).await?;
        // 扫描期间被关闭：现在装 watcher 会泄漏它们。
        if inner.state.lock().expect("state").closed {
            break;
        }
        let next = scan.snapshot.clone();
        let (diff_paths, mode) = {
            let mut state = inner.state.lock().expect("state");
            let targets = state.targets.clone();
            state.linked_files = scan.linked_files.clone();
            let diff = if report {
                diff(&state.snapshot, &next, &targets)
            } else {
                Vec::new()
            };
            state.snapshot = next;
            (diff, state.mode)
        };
        for path in diff_paths {
            changed.insert(path);
        }
        if mode == WatchMode::Polling || !reconcile_watchers(inner, &scan) {
            break;
        }
        if cfg!(target_os = "macos") {
            schedule_settle(inner);
        }
        // 新 watcher 建立之前写入新目录的内容会在下一轮显示出来。
        report = true;
    }
    Ok(changed)
}

/// 对应 `#diff`。
fn diff(previous: &Snapshot, next: &Snapshot, targets: &[ResolvedTarget]) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, entry) in next {
        match previous.get(path) {
            Some(before) if same_entry(before, entry) => {}
            _ => changed.push(reported(path, targets)),
        }
    }
    for path in previous.keys() {
        if !next.contains_key(path) {
            changed.push(reported(path, targets));
        }
    }
    changed
}

/// 对应 `#scan`。
async fn scan(inner: &Arc<Inner>) -> Result<Scan, FileError> {
    let (targets, mode, max_directories) = {
        let state = inner.state.lock().expect("state");
        (state.targets.clone(), state.mode, state.max_directories)
    };

    let mut snapshot: Snapshot = HashMap::new();
    let mut linked_files: HashSet<String> = HashSet::new();
    // 目录只计数一次，但每个目标都遍历一次：重叠目标在递归与排除上不同，为某个目标记录的条目
    // 仍要为另一个目标继续下钻。
    let mut counted: HashSet<String> = HashSet::new();
    let mut traversed: HashSet<String> = HashSet::new();
    // 列出条目的类型（不跟随链接）：链接到目录的目标由 `stat` 记录，但递归目标下的符号链接不跟随。
    let mut listed: HashMap<String, EntryKind> = HashMap::new();

    for (index, target) in targets.iter().enumerate() {
        for ancestor in ancestors_of(&target.path) {
            if snapshot.contains_key(&ancestor) {
                continue;
            }
            if let Ok(stats) = tokio::fs::symlink_metadata(&ancestor).await {
                // 只比较身份：祖先自身的时间戳会随每个无关兄弟一起变化。
                let mut entry = entry_of(&stats, None);
                entry.size = 0;
                entry.mtime_ms = 0.0;
                snapshot.insert(ancestor, entry);
            }
        }
        // 目标本身可能是「被监听对象」的符号链接：跟随它。缺失的目标监听其创建；
        // 因权限不足无法到达的目标则失败。
        let stats = match tokio::fs::metadata(&target.path).await {
            Ok(stats) => stats,
            Err(error) => {
                if is_denied(&error) {
                    return Err(to_file_error(error, Some(&target.path)));
                }
                continue;
            }
        };
        record(target.path.clone(), &stats, mode, &mut snapshot).await;
        let is_symlink = tokio::fs::symlink_metadata(&target.path)
            .await
            .map(|stats| stats.file_type().is_symlink())
            .unwrap_or(false);
        if stats.is_file() && is_symlink {
            linked_files.insert(target.path.clone());
        }
        if stats.is_dir() {
            counted.insert(target.path.clone());
            if counted.len() > max_directories {
                return Err(budget_exceeded(max_directories));
            }
            scan_directory(
                index,
                target,
                target.path.clone(),
                mode,
                max_directories,
                &mut snapshot,
                &mut listed,
                &mut counted,
                &mut traversed,
            )
            .await?;
        }
    }
    Ok(Scan {
        snapshot,
        linked_files,
    })
}

/// 对应 `#reconcileWatchers`：监视每个目标的每个现存祖先、每个目标目录、每个指向文件的符号链接，
/// 以及 Linux 上递归目标下的每个目录（其它平台每个递归目标一个 watcher）。返回是否新增了 watcher。
fn reconcile_watchers(inner: &Arc<Inner>, scan: &Scan) -> bool {
    let snapshot = &scan.snapshot;
    let linked_files = &scan.linked_files;
    let mut wanted: HashMap<String, bool> = HashMap::new();
    for path in linked_files {
        wanted.insert(path.clone(), false);
    }
    let per_directory = cfg!(target_os = "linux") || cfg!(target_os = "android");
    let targets = inner.state.lock().expect("state").targets.clone();

    for target in &targets {
        for ancestor in ancestors_of(&target.path) {
            if snapshot
                .get(&ancestor)
                .map(|e| e.kind == EntryKind::Directory)
                .unwrap_or(false)
                && !wanted.contains_key(&ancestor)
            {
                wanted.insert(ancestor, false);
            }
        }
        if snapshot.get(&target.path).map(|e| e.kind) != Some(EntryKind::Directory) {
            continue;
        }
        let recursive_root = target.recursive && !per_directory;
        wanted.insert(target.path.clone(), recursive_root);
        if target.recursive && per_directory {
            for (path, entry) in snapshot {
                if entry.kind == EntryKind::Directory
                    && path != &target.path
                    && is_within(path, &target.path)
                {
                    wanted.insert(path.clone(), false);
                }
            }
        }
    }

    // 已消失或被替换：watcher 跟随它安装时所在的目录，而不是路径。
    let to_remove: Vec<String> = {
        let state = inner.state.lock().expect("state");
        state
            .watchers
            .iter()
            .filter(|(path, installed)| {
                let entry = snapshot.get(*path);
                !wanted.contains_key(*path)
                    || entry.is_none_or(|e| e.dev != installed.dev || e.ino != installed.ino)
            })
            .map(|(path, _)| path.clone())
            .collect()
    };
    for path in &to_remove {
        if let Some(watcher) = inner.watcher.lock().expect("watcher").as_mut() {
            let _ = watcher.unwatch(Path::new(path));
        }
        inner.state.lock().expect("state").watchers.remove(path);
    }

    let mut added = false;
    for (path, recursive) in wanted {
        if inner
            .state
            .lock()
            .expect("state")
            .watchers
            .contains_key(&path)
        {
            continue;
        }
        let Some(entry) = snapshot.get(&path).cloned() else {
            continue;
        };
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let result = inner
            .watcher
            .lock()
            .expect("watcher")
            .as_mut()
            .expect("watcher installed")
            .watch(Path::new(&path), mode);
        match result {
            Ok(()) => {
                inner.state.lock().expect("state").watchers.insert(
                    path,
                    Installed {
                        dev: entry.dev,
                        ino: entry.ino,
                    },
                );
                added = true;
            }
            Err(error) => {
                // 超出 watch 数量或不支持：此后改为快照比较，并声明覆盖范围不确定。
                if reconcile_should_poll(&error) {
                    switch_to_polling(inner);
                    return false;
                }
            }
        }
    }
    added
}

/// 对应 `#switchToPolling`。
fn switch_to_polling(inner: &Arc<Inner>) {
    {
        let mut state = inner.state.lock().expect("state");
        if state.mode == WatchMode::Polling {
            return;
        }
        state.mode = WatchMode::Polling;
        state.watchers.clear();
        state.timer_present = false;
    }
    inner.watcher.lock().expect("watcher").take();
    deliver(inner, WatchChange::Overflow);
    schedule_poll(inner);
}

/// 对应 `record`。
async fn record(path: String, stats: &std::fs::Metadata, mode: WatchMode, snapshot: &mut Snapshot) {
    let hash = if mode == WatchMode::Polling
        && stats.is_file()
        && stats.len() <= HASH_MAX_BYTES
        && is_recent(stats)
    {
        match tokio::fs::read(&path).await {
            Ok(content) => Some(sha256_hex(&content)),
            Err(_) => None,
        }
    } else {
        None
    };
    snapshot.insert(path, entry_of(stats, hash.as_deref()));
}

/// 对应 `scanDirectory` 的递归体（`BoxFuture` 支撑 async 递归）。
///
/// 参数多是因为忠实于上游闭包捕获的九个变量（index/target/directory/mode/maxDirectories/
/// snapshot/listed/counted/traversed）。
#[allow(clippy::too_many_arguments)]
fn scan_directory<'a>(
    index: usize,
    target: &'a ResolvedTarget,
    directory: String,
    mode: WatchMode,
    max_directories: usize,
    snapshot: &'a mut Snapshot,
    listed: &'a mut HashMap<String, EntryKind>,
    counted: &'a mut HashSet<String>,
    traversed: &'a mut HashSet<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), FileError>> + Send + 'a>> {
    Box::pin(async move {
        let key = format!("{index}\0{directory}");
        if !traversed.insert(key) {
            return Ok(());
        }

        let mut readdir = match tokio::fs::read_dir(&directory).await {
            Ok(rd) => rd,
            Err(error) => {
                // 被监听的目录自身必须可读；其下不可读的目录跳过。
                if directory == target.path && is_denied(&error) {
                    return Err(to_file_error(error, Some(&directory)));
                }
                if is_node_error_kind(
                    &error,
                    &[libc::ENOENT, libc::EACCES, libc::EPERM, libc::ENOTDIR],
                ) {
                    return Ok(());
                }
                return Err(to_file_error(error, Some(&directory)));
            }
        };

        let mut names: Vec<String> = Vec::new();
        loop {
            match readdir.next_entry().await {
                Ok(Some(entry)) => names.push(entry.file_name().to_string_lossy().into_owned()),
                Ok(None) => break,
                Err(error) => return Err(to_file_error(error, Some(&directory))),
            }
        }

        for name in names {
            if excluded(target, &name) {
                continue;
            }
            let path = join_path(&directory, &name);
            let kind = match listed.get(&path) {
                Some(kind) => *kind,
                None => {
                    let stats = match tokio::fs::symlink_metadata(&path).await {
                        Ok(stats) => stats,
                        Err(_) => continue,
                    };
                    let entry = entry_of(&stats, None);
                    let kind = entry.kind;
                    listed.insert(path.clone(), kind);
                    // 目标自身的条目（跟随链接）优先于另一个目标对它的列表。
                    if !snapshot.contains_key(&path) {
                        record(path.clone(), &stats, mode, snapshot).await;
                    }
                    kind
                }
            };
            if target.recursive && kind == EntryKind::Directory {
                counted.insert(path.clone());
                if counted.len() > max_directories {
                    return Err(budget_exceeded(max_directories));
                }
                scan_directory(
                    index,
                    target,
                    path.clone(),
                    mode,
                    max_directories,
                    snapshot,
                    listed,
                    counted,
                    traversed,
                )
                .await?;
            }
        }
        Ok(())
    })
}
