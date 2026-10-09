//! 内存文件系统：供测试与本地验证使用的 [`crate::env::FileSystem`] 实现。
//!
//! 上游的 `env/node.ts` 提供真实的 Node 实现（1,226 行）；这里只实现与持久化后端相关的最小面，
//! 其余方法返回「不支持」。它不是产品代码，但放在 `src` 下以便集成测试复用。

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::chord::context::Context;
use crate::env::{
    BinaryReader, DirReader, FileError, FileErrorCode, FileInfo, FileKind, FileSystem, FileWatcher,
    LineScan, RemoveOptions, TextLineReader, WatchChange, WatchTarget,
};

#[derive(Default)]
struct State {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeMap<String, ()>,
}

/// 内存文件系统。
pub struct InMemoryFileSystem {
    id: String,
    cwd: String,
    state: Mutex<State>,
}

impl Default for InMemoryFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryFileSystem {
    /// 新建一个以 `/` 为根的实例。
    pub fn new() -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert("/".to_string(), ());
        Self {
            id: "memory".to_string(),
            cwd: "/".to_string(),
            state: Mutex::new(State {
                files: BTreeMap::new(),
                dirs,
            }),
        }
    }

    fn not_found(path: &str) -> FileError {
        FileError::new(
            FileErrorCode::NotFound,
            format!("{path} does not exist"),
            Some(path.to_string()),
        )
    }

    /// 测试辅助：写入一个文件（不经过 trait）。
    pub fn put(&self, path: &str, content: &str) {
        let mut state = self.state.lock().expect("fs");
        state
            .files
            .insert(path.to_string(), content.as_bytes().to_vec());
    }

    /// 测试辅助：读取一个文件。
    pub fn get(&self, path: &str) -> Option<String> {
        let state = self.state.lock().expect("fs");
        state
            .files
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }
}

fn unsupported<T>() -> Result<T, FileError> {
    Err(FileError::new(
        FileErrorCode::NotSupported,
        "not supported by the in-memory file system",
        None,
    ))
}

#[async_trait::async_trait]
impl FileSystem for InMemoryFileSystem {
    fn id(&self) -> &str {
        &self.id
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    async fn absolute_path(&self, path: &str, _context: &dyn Context) -> Result<String, FileError> {
        Ok(if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{path}", self.cwd.trim_end_matches('/'))
        })
    }

    async fn join_path(&self, parts: &[&str], _context: &dyn Context) -> Result<String, FileError> {
        Ok(parts.join("/"))
    }

    async fn read_text_file(
        &self,
        path: &str,
        _context: &dyn Context,
    ) -> Result<String, FileError> {
        let state = self.state.lock().expect("fs");
        state
            .files
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .ok_or_else(|| Self::not_found(path))
    }

    async fn open_text_line_reader(
        &self,
        _path: &str,
        _context: &dyn Context,
    ) -> Result<Box<dyn TextLineReader>, FileError> {
        unsupported()
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &dyn Context,
    ) -> Result<Vec<String>, FileError> {
        let content = self.read_text_file(path, context).await?;
        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        if let Some(max) = max_lines {
            lines.truncate(max);
        }
        Ok(lines)
    }

    async fn read_binary_file(
        &self,
        path: &str,
        _context: &dyn Context,
    ) -> Result<Vec<u8>, FileError> {
        let state = self.state.lock().expect("fs");
        state
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| Self::not_found(path))
    }

    async fn open_binary_reader(
        &self,
        _path: &str,
        _no_follow: Option<bool>,
        _context: &dyn Context,
    ) -> Result<Box<dyn BinaryReader>, FileError> {
        unsupported()
    }

    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        _context: &dyn Context,
    ) -> Result<(), FileError> {
        let mut state = self.state.lock().expect("fs");
        state.files.insert(path.to_string(), content.to_vec());
        Ok(())
    }

    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        _context: &dyn Context,
    ) -> Result<(), FileError> {
        let mut state = self.state.lock().expect("fs");
        state
            .files
            .entry(path.to_string())
            .or_default()
            .extend_from_slice(content);
        Ok(())
    }

    async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        _context: &dyn Context,
    ) -> Result<(), FileError> {
        let mut state = self.state.lock().expect("fs");
        let entry = state.files.entry(path.to_string()).or_default();
        entry.resize(size as usize, 0);
        Ok(())
    }

    async fn flush_file(&self, _path: &str, _context: &dyn Context) -> Result<(), FileError> {
        Ok(())
    }

    async fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
        _context: &dyn Context,
    ) -> Result<(), FileError> {
        let mut state = self.state.lock().expect("fs");
        let Some(content) = state.files.remove(source_path) else {
            return Err(Self::not_found(source_path));
        };
        state.files.insert(destination_path.to_string(), content);
        Ok(())
    }

    async fn file_info(&self, path: &str, _context: &dyn Context) -> Result<FileInfo, FileError> {
        let state = self.state.lock().expect("fs");
        if let Some(bytes) = state.files.get(path) {
            return Ok(FileInfo {
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: path.to_string(),
                kind: FileKind::File,
                size: bytes.len() as u64,
                mtime_ms: 0.0,
            });
        }
        if state.dirs.contains_key(path) {
            return Ok(FileInfo {
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: path.to_string(),
                kind: FileKind::Directory,
                size: 0,
                mtime_ms: 0.0,
            });
        }
        Err(Self::not_found(path))
    }

    async fn list_dir(
        &self,
        path: &str,
        _context: &dyn Context,
    ) -> Result<Vec<FileInfo>, FileError> {
        let state = self.state.lock().expect("fs");
        let prefix = format!("{}/", path.trim_end_matches('/'));
        Ok(state
            .files
            .keys()
            .filter(|candidate| candidate.starts_with(&prefix))
            .map(|candidate| FileInfo {
                name: candidate
                    .rsplit('/')
                    .next()
                    .unwrap_or(candidate)
                    .to_string(),
                path: candidate.clone(),
                kind: FileKind::File,
                size: state.files[candidate].len() as u64,
                mtime_ms: 0.0,
            })
            .collect())
    }

    async fn open_dir_reader(
        &self,
        _path: &str,
        _context: &dyn Context,
    ) -> Result<Box<dyn DirReader>, FileError> {
        unsupported()
    }

    async fn watch(
        &self,
        _targets: &[WatchTarget],
        _on_change: Box<dyn Fn(WatchChange) + Send + Sync>,
        _context: &dyn Context,
    ) -> Result<Box<dyn FileWatcher>, FileError> {
        unsupported()
    }

    async fn canonical_path(
        &self,
        path: &str,
        _context: &dyn Context,
    ) -> Result<String, FileError> {
        Ok(path.to_string())
    }

    async fn exists(&self, path: &str, _context: &dyn Context) -> Result<bool, FileError> {
        let state = self.state.lock().expect("fs");
        Ok(state.files.contains_key(path) || state.dirs.contains_key(path))
    }

    async fn create_dir(
        &self,
        path: &str,
        _recursive: Option<bool>,
        _context: &dyn Context,
    ) -> Result<(), FileError> {
        self.state
            .lock()
            .expect("fs")
            .dirs
            .insert(path.to_string(), ());
        Ok(())
    }

    async fn remove(
        &self,
        path: &str,
        _options: RemoveOptions,
        _context: &dyn Context,
    ) -> Result<(), FileError> {
        let mut state = self.state.lock().expect("fs");
        state.files.remove(path);
        state.dirs.remove(path);
        Ok(())
    }

    async fn create_temp_dir(
        &self,
        _prefix: Option<&str>,
        _context: &dyn Context,
    ) -> Result<String, FileError> {
        unsupported()
    }

    async fn create_temp_file(
        &self,
        _prefix: Option<&str>,
        _suffix: Option<&str>,
        _context: &dyn Context,
    ) -> Result<String, FileError> {
        unsupported()
    }

    async fn cleanup(&self, _context: &dyn Context) {}
}

// `LineScan` 仅用于 `BinaryReader::scan_lines` 的返回类型；此处引用以保持导入完整。
#[allow(dead_code)]
fn _line_scan_marker(_: LineScan) {}
