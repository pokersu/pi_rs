//! 对应 `src/env/`：可移植的文件系统与 Shell 能力。
//!
//! 已落地：契约层（[`index`]）、流式解码（[`decode`]）与行扫描（[`line_scan`]）。
//! `node`（Node 实现）与 `node-watch` 待落地，见 `UPSTREAM-SYNC-v1.1.0.md`。

pub mod decode;
pub mod index;
pub mod line_scan;
pub mod node;
pub mod node_watch;
pub mod testing;

pub use node::NodeExecutionEnv;
pub use node_watch::{NodeFileWatcher, NodeWatchOptions};
pub use testing::InMemoryFileSystem;

pub use index::{
    BinaryReader, DirReader, ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError,
    FileErrorCode, FileInfo, FileKind, FileSystem, FileWatcher, LineScan, RemoveOptions, Shell,
    ShellCommand, ShellExecOptions, ShellExecResult, ShellOutputCallback, ShellOutputInfo,
    ShellOutputSkip, ShellOutputStream, ShellOutputWindow, ShellSpillOptions, TextLine,
    TextLineReader, WatchChange, WatchExclude, WatchMode, WatchTarget,
};
pub use line_scan::LineScanner;
