//! `env::node` 的端到端测试：真实文件系统与 Shell。

use std::sync::{Arc, Mutex};

use pi_durable::chord::context::{Context, EmptyContext};
use pi_durable::env::{
    FileKind, FileSystem, NodeExecutionEnv, RemoveOptions, Shell, ShellCommand, ShellExecOptions,
};

fn context() -> Arc<dyn Context> {
    Arc::new(EmptyContext::new("[test env node]"))
}

#[tokio::test]
async fn writes_reads_and_lists_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().to_string_lossy().into_owned();
    let env = NodeExecutionEnv::new(cwd);

    env.write_file("hello.txt", b"one\ntwo\nthree", context().as_ref())
        .await
        .expect("write");

    let text = env
        .read_text_file("hello.txt", context().as_ref())
        .await
        .expect("read");
    assert_eq!(text, "one\ntwo\nthree");

    assert!(
        env.exists("hello.txt", context().as_ref())
            .await
            .expect("exists")
    );
    assert!(
        !env.exists("missing.txt", context().as_ref())
            .await
            .expect("exists")
    );

    let info = env
        .file_info("hello.txt", context().as_ref())
        .await
        .expect("info");
    assert_eq!(info.kind, FileKind::File);
    assert_eq!(info.size, 13);

    let entries = env.list_dir(".", context().as_ref()).await.expect("list");
    assert!(entries.iter().any(|entry| entry.name == "hello.txt"));
}

#[tokio::test]
async fn binary_reader_scans_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = NodeExecutionEnv::new(dir.path().to_string_lossy().into_owned());

    env.write_file("lines.txt", b"alpha\nbeta\ngamma", context().as_ref())
        .await
        .expect("write");

    let reader = env
        .open_binary_reader("lines.txt", None, context().as_ref())
        .await
        .expect("reader");

    let bytes = reader.read(0, 5, context().as_ref()).await.expect("read");
    assert_eq!(bytes, b"alpha");

    let scan = reader
        .scan_lines(1, Some(2), context().as_ref())
        .await
        .expect("scan");
    assert_eq!(scan.selected_bytes, 4); // "beta"
    assert_eq!(scan.newlines, 2);

    reader.close(context().as_ref()).await;
}

#[tokio::test]
async fn shell_runs_a_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = NodeExecutionEnv::new(dir.path().to_string_lossy().into_owned());

    let output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&output);
    let result = env
        .exec(
            ShellCommand::Shell("printf 'hello' && exit 3".to_string()),
            ShellExecOptions {
                on_output: Some(Box::new(move |text, _context, _info| {
                    sink.lock().expect("output").push_str(text);
                })),
                ..Default::default()
            },
            context().as_ref(),
        )
        .await
        .expect("exec");
    assert_eq!(result.exit_code, 3);
    assert_eq!(output.lock().expect("output").as_str(), "hello");
}

#[tokio::test]
async fn remove_respects_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = NodeExecutionEnv::new(dir.path().to_string_lossy().into_owned());

    env.write_file("gone.txt", b"x", context().as_ref())
        .await
        .expect("write");
    env.remove(
        "gone.txt",
        RemoveOptions {
            recursive: None,
            force: None,
        },
        context().as_ref(),
    )
    .await
    .expect("remove");
    assert!(
        !env.exists("gone.txt", context().as_ref())
            .await
            .expect("exists")
    );

    // force 删除不存在的文件不报错。
    env.remove(
        "absent.txt",
        RemoveOptions {
            recursive: None,
            force: Some(true),
        },
        context().as_ref(),
    )
    .await
    .expect("force remove");
}
