//! Rust 翻译自 edit 工具测试（edit-diff 算法 + edit 工具端到端）。

use std::sync::Arc;

use pi_agent::harness::tools::create_edit_tool;
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

#[test]
fn edit_diff_replaces_unique_text() {
    let content = "hello\nworld\nfoo";
    let edits = vec![pi_agent::harness::tools::edit_diff::Edit {
        old_text: "world".into(),
        new_text: "universe".into(),
    }];
    let result = pi_agent::harness::tools::edit_diff::apply_edits_to_normalized_content(
        content, &edits, "test.txt",
    );
    assert_eq!(result.new_content, "hello\nuniverse\nfoo");
}

#[test]
fn edit_diff_rejects_duplicate_text() {
    let content = "a\na\nb";
    let edits = vec![pi_agent::harness::tools::edit_diff::Edit {
        old_text: "a".into(),
        new_text: "x".into(),
    }];
    let result = std::panic::catch_unwind(|| {
        pi_agent::harness::tools::edit_diff::apply_edits_to_normalized_content(
            content, &edits, "test.txt",
        )
    });
    assert!(result.is_err());
}

#[tokio::test]
async fn edit_tool_edits_file() {
    let env = Arc::new(NodeExecutionEnv::new(
        std::env::temp_dir().to_string_lossy().to_string(),
    ));
    let dir = env
        .create_temp_dir(Some("pi-agent-edit-"), None)
        .await
        .unwrap();
    let file = format!("{dir}/target.txt");
    env.write_file(&file, b"fn main() {\n    println!(\"old\");\n}", None)
        .await
        .unwrap();

    let tool = create_edit_tool(env.clone());
    let result = (tool.execute)(
        "1".into(),
        serde_json::json!({
            "path": file.clone(),
            "edits": [{ "oldText": "old", "newText": "new" }]
        }),
        None,
        None,
    )
    .await;

    assert!(text_of(&result.content).contains("Successfully replaced 1 block(s)"));
    assert!(result.details["patch"].as_str().unwrap().contains("new"));

    let read_back = env.read_text_file(&file, None).await.unwrap();
    assert!(read_back.contains("println!(\"new\")"));

    env.remove(&dir, true, true, None).await.unwrap();
}
