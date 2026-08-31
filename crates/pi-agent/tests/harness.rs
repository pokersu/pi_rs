//! Rust 翻译自 harness 测试（NodeExecutionEnv + read/write/bash 工具）。

use std::sync::Arc;

use pi_agent::harness::tools::{create_bash_tool, create_read_tool, create_write_tool};
use pi_agent::harness::{FileSystem, NodeExecutionEnv};
use pi_ai::TextOrImageContent;

fn text_of(content: &[TextOrImageContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            TextOrImageContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

async fn temp_dir(env: &Arc<NodeExecutionEnv>) -> String {
    env.create_temp_dir(Some("pi-agent-test-"), None)
        .await
        .unwrap()
}

#[tokio::test]
async fn node_env_writes_and_reads() {
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let dir = temp_dir(&env).await;
    let file = format!("{dir}/test.txt");
    env.write_file(&file, b"hello\nworld", None).await.unwrap();
    let text = env.read_text_file(&file, None).await.unwrap();
    assert_eq!(text, "hello\nworld");
    env.remove(&dir, true, true, None).await.unwrap();
}

#[tokio::test]
async fn read_tool_reads_text_file() {
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let dir = temp_dir(&env).await;
    let file = format!("{dir}/test.txt");
    env.write_file(&file, b"line1\nline2\nline3", None)
        .await
        .unwrap();

    let tool = create_read_tool(env.clone());
    let result = (tool.execute)(
        "1".into(),
        serde_json::json!({ "path": file.clone() }),
        None,
        None,
    )
    .await;
    let text = text_of(&result.content);
    assert!(text.contains("line1"));
    assert!(text.contains("line3"));

    env.remove(&dir, true, true, None).await.unwrap();
}

#[tokio::test]
async fn read_tool_supports_offset() {
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let dir = temp_dir(&env).await;
    let file = format!("{dir}/test.txt");
    env.write_file(&file, b"a\nb\nc\nd", None).await.unwrap();

    let tool = create_read_tool(env.clone());
    let result = (tool.execute)(
        "1".into(),
        serde_json::json!({ "path": file.clone(), "offset": 3, "limit": 1 }),
        None,
        None,
    )
    .await;
    let text = text_of(&result.content);
    assert!(text.contains('c'));
    assert!(!text.contains('d'));

    env.remove(&dir, true, true, None).await.unwrap();
}

#[tokio::test]
async fn write_tool_writes_file() {
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let dir = temp_dir(&env).await;
    let file = format!("{dir}/out.txt");

    let tool = create_write_tool(env.clone());
    let result = (tool.execute)(
        "1".into(),
        serde_json::json!({ "path": file.clone(), "content": "written content" }),
        None,
        None,
    )
    .await;
    assert!(text_of(&result.content).contains("Successfully wrote"));

    let read_back = env.read_text_file(&file, None).await.unwrap();
    assert_eq!(read_back, "written content");

    env.remove(&dir, true, true, None).await.unwrap();
}

#[tokio::test]
async fn bash_tool_runs_command() {
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let tool = create_bash_tool(env.clone());
    let result = (tool.execute)(
        "1".into(),
        serde_json::json!({ "command": "echo hello" }),
        None,
        None,
    )
    .await;
    let text = text_of(&result.content);
    assert!(text.contains("hello"));
}

#[tokio::test]
async fn bash_tool_reports_nonzero_exit() {
    use futures::FutureExt;
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let tool = create_bash_tool(env.clone());
    let result = std::panic::AssertUnwindSafe((tool.execute)(
        "1".into(),
        serde_json::json!({ "command": "exit 3" }),
        None,
        None,
    ))
    .catch_unwind()
    .await;
    assert!(result.is_err());
}
