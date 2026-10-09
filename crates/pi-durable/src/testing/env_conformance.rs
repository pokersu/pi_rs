//! 对应 `testing/env-conformance.ts`：`ExecutionEnv` 的参数化一致性测试（核心子集）。
//!
//! 断言用 Rust 原生 `assert!` / `assert_eq!`；case 接收一个 `&dyn ExecutionEnv`，
//! 由测试方提供（如 `NodeExecutionEnv` 配一个临时目录）。

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::chord::context::Context;
use crate::env::{
    ExecutionEnv, FileKind, ShellCommand, ShellExecOptions, ShellExecResult, ShellOutputInfo,
};

/// 一个一致性测试 case：名称 + 以某环境运行的函数。
pub type EnvConformanceCase = (
    &'static str,
    Arc<dyn for<'a> Fn(&'a dyn ExecutionEnv, &'a dyn Context) -> BoxFuture<'a, ()> + Send + Sync>,
);

struct Collected {
    result: ShellExecResult,
    stdout: String,
    stderr: String,
}

async fn exec_collect(
    env: &dyn ExecutionEnv,
    command: ShellCommand,
    cwd: Option<&str>,
    context: &dyn Context,
) -> Collected {
    let stdout = Arc::new(Mutex::new(String::new()));
    let stderr = Arc::new(Mutex::new(String::new()));
    let stdout_sink = Arc::clone(&stdout);
    let stderr_sink = Arc::clone(&stderr);
    let result = env
        .exec(
            command,
            ShellExecOptions {
                cwd: cwd.map(str::to_string),
                on_output: Some(Box::new(
                    move |text, _ctx, info: &ShellOutputInfo| match info.stream {
                        crate::env::ShellOutputStream::Stdout => {
                            stdout_sink.lock().expect("stdout").push_str(text)
                        }
                        crate::env::ShellOutputStream::Stderr => {
                            stderr_sink.lock().expect("stderr").push_str(text)
                        }
                    },
                )),
                ..Default::default()
            },
            context,
        )
        .await
        .expect("exec");
    Collected {
        result,
        stdout: stdout.lock().expect("stdout").clone(),
        stderr: stderr.lock().expect("stderr").clone(),
    }
}

/// 对应 `createEnvConformance`：核心一致性 case 集合。
pub fn env_conformance_cases() -> Vec<EnvConformanceCase> {
    vec![
        (
            "binary reader reads byte ranges of the opened file",
            Arc::new(|env, context| {
                Box::pin(async move {
                    env.write_file("data.txt", b"hello world", context)
                        .await
                        .expect("write");
                    let reader = env
                        .open_binary_reader("data.txt", None, context)
                        .await
                        .expect("reader");
                    let info = reader.info(context).await.expect("info");
                    assert_eq!(info.name, "data.txt");
                    assert_eq!(info.kind, FileKind::File);
                    assert_eq!(info.size, 11);
                    assert_eq!(reader.read(0, 5, context).await.expect("read"), b"hello");
                    assert_eq!(reader.read(6, 100, context).await.expect("read"), b"world");
                    assert!(reader.read(11, 4, context).await.expect("read").is_empty());
                    assert!(reader.read(50, 1, context).await.expect("read").is_empty());
                    assert!(reader.read(3, 0, context).await.expect("read").is_empty());
                    reader.close(context).await;
                    reader.close(context).await;
                })
            }),
        ),
        (
            "binary reader scans lines like decoding the whole file",
            Arc::new(|env, context| {
                Box::pin(async move {
                    // BOM + 换行前的无效序列 + 空行 + 后续 U+FEFF + 无结尾换行。
                    let bytes: Vec<u8> = vec![
                        0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf,
                        0x62, 0x0a, 0xc3, 0xa9,
                    ];
                    env.write_file("lines.txt", &bytes, context)
                        .await
                        .expect("write");
                    let reader = env
                        .open_binary_reader("lines.txt", None, context)
                        .await
                        .expect("reader");

                    for (start, end) in [
                        (0, None),
                        (0, Some(1)),
                        (1, Some(3)),
                        (2, Some(3)),
                        (3, None),
                        (4, Some(9)),
                    ] {
                        let scan = reader.scan_lines(start, end, context).await.expect("scan");
                        // 与上游 `new TextDecoder().decode(bytes)` 一致：剥离前导 BOM。
                        let mut decoder = crate::env::decode::StreamDecoder::new();
                        let mut decoded = decoder.decode(Some(&bytes));
                        decoded.push_str(&decoder.decode(None));
                        let lines: Vec<&str> = decoded.split('\n').collect();
                        // JS `slice` 越界 clamp 到 len。
                        let selected = lines[start.min(lines.len())
                            ..end.map_or(lines.len(), |e| e.min(lines.len()))]
                            .join("\n");
                        // 对应 `range(from, to)`：`from == 0` 时剥离 BOM，否则保留。
                        let range_text = {
                            let slice = &bytes[scan.start..scan.end];
                            if scan.start == 0 && crate::env::decode::starts_with_bom(slice) {
                                String::from_utf8_lossy(&slice[3..]).into_owned()
                            } else {
                                String::from_utf8_lossy(slice).into_owned()
                            }
                        };
                        assert_eq!(range_text, selected, "range for {start:?}..{end:?}");
                        assert_eq!(scan.selected_bytes, selected.len());
                        assert_eq!(scan.newlines, lines.len() - 1);
                    }
                })
            }),
        ),
        (
            "binary reader keeps reading the file it opened after a rename",
            Arc::new(|env, context| {
                Box::pin(async move {
                    env.write_file("a.txt", b"one", context)
                        .await
                        .expect("write");
                    let reader = env
                        .open_binary_reader("a.txt", None, context)
                        .await
                        .expect("reader");
                    env.rename_file("a.txt", "b.txt", context)
                        .await
                        .expect("rename");
                    env.write_file("a.txt", b"two", context)
                        .await
                        .expect("write");
                    assert_eq!(reader.read(0, 10, context).await.expect("read"), b"one");
                })
            }),
        ),
        (
            "binary reader refuses directories and missing files",
            Arc::new(|env, context| {
                Box::pin(async move {
                    env.create_dir("dir", None, context).await.expect("mkdir");
                    let dir_result = env.open_binary_reader("dir", None, context).await;
                    assert_eq!(
                        dir_result.err().map(|e| e.code),
                        Some(crate::env::FileErrorCode::IsDirectory)
                    );
                    let missing = env.open_binary_reader("missing.txt", None, context).await;
                    assert_eq!(
                        missing.err().map(|e| e.code),
                        Some(crate::env::FileErrorCode::NotFound)
                    );
                })
            }),
        ),
        (
            "directory reader pages every entry exactly once",
            Arc::new(|env, context| {
                Box::pin(async move {
                    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
                    for name in names {
                        env.write_file(name, name.as_bytes(), context)
                            .await
                            .expect("write");
                    }
                    env.create_dir("sub", None, context).await.expect("mkdir");
                    let reader = env.open_dir_reader(".", context).await.expect("reader");
                    let mut all: Vec<String> = Vec::new();
                    let mut done = false;
                    for _ in 0..1000 {
                        let (entries, page_done) = reader.next(2, context).await.expect("next");
                        assert!(entries.len() <= 2);
                        all.extend(entries.iter().map(|e| e.name.clone()));
                        if page_done {
                            done = true;
                            break;
                        }
                    }
                    assert!(done);
                    all.sort();
                    let mut expected = names.to_vec();
                    expected.push("sub");
                    expected.sort();
                    assert_eq!(all, expected);
                })
            }),
        ),
        (
            "argv exec passes arguments to the program without shell parsing",
            Arc::new(|env, context| {
                Box::pin(async move {
                    let hostile = "it's $(touch pwned) `touch pwned` *; touch pwned";
                    let collected = exec_collect(
                        env,
                        ShellCommand::Argv(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            "printf \"%s|%s\" \"$1\" \"$2\"".to_string(),
                            "argv0".to_string(),
                            hostile.to_string(),
                            "a b".to_string(),
                        ]),
                        None,
                        context,
                    )
                    .await;
                    assert_eq!(collected.result.exit_code, 0);
                    assert_eq!(collected.stdout, format!("{hostile}|a b"));
                    assert!(!env.exists("pwned", context).await.expect("exists"));
                })
            }),
        ),
        (
            "exec reports the stream of every chunk in both forms",
            Arc::new(|env, context| {
                Box::pin(async move {
                    let script = "printf out; printf err >&2; printf more";
                    let argv = exec_collect(
                        env,
                        ShellCommand::Argv(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            script.to_string(),
                        ]),
                        None,
                        context,
                    )
                    .await;
                    assert_eq!(argv.result.exit_code, 0);
                    assert_eq!(argv.stdout, "outmore");
                    assert_eq!(argv.stderr, "err");

                    let string =
                        exec_collect(env, ShellCommand::Shell(script.to_string()), None, context)
                            .await;
                    assert_eq!(string.result.exit_code, 0);
                    assert_eq!(string.stdout, "outmore");
                    assert_eq!(string.stderr, "err");
                })
            }),
        ),
        (
            "argv exec honors cwd and exit codes",
            Arc::new(|env, context| {
                Box::pin(async move {
                    env.create_dir("sub", None, context).await.expect("mkdir");
                    let made = exec_collect(
                        env,
                        ShellCommand::Argv(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            "printf x > made.txt; exit 3".to_string(),
                        ]),
                        Some("sub"),
                        context,
                    )
                    .await;
                    assert_eq!(made.result.exit_code, 3);
                    assert_eq!(
                        env.read_text_file("sub/made.txt", context)
                            .await
                            .expect("read"),
                        "x"
                    );
                })
            }),
        ),
        (
            "argv exec reports missing programs and empty argv as spawn errors",
            Arc::new(|env, context| {
                Box::pin(async move {
                    let missing = env
                        .exec(
                            ShellCommand::Argv(vec![
                                "pi-durable-conformance-missing-program".to_string(),
                            ]),
                            ShellExecOptions::default(),
                            context,
                        )
                        .await;
                    assert_eq!(
                        missing.err().map(|e| e.code),
                        Some(crate::env::ExecutionErrorCode::SpawnError)
                    );
                    let empty = env
                        .exec(
                            ShellCommand::Argv(vec![]),
                            ShellExecOptions::default(),
                            context,
                        )
                        .await;
                    assert_eq!(
                        empty.err().map(|e| e.code),
                        Some(crate::env::ExecutionErrorCode::SpawnError)
                    );
                })
            }),
        ),
        (
            "argv exec distinguishes timeout from abort",
            Arc::new(|env, context| {
                Box::pin(async move {
                    let timed_out = env
                        .exec(
                            ShellCommand::Argv(vec![
                                "sh".to_string(),
                                "-c".to_string(),
                                "sleep 2".to_string(),
                            ]),
                            ShellExecOptions {
                                timeout: Some(0.1),
                                ..Default::default()
                            },
                            context,
                        )
                        .await;
                    assert_eq!(
                        timed_out.err().map(|e| e.code),
                        Some(crate::env::ExecutionErrorCode::Timeout)
                    );
                })
            }),
        ),
    ]
}
