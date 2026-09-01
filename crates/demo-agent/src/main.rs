//! demo-agent：交互式 REPL 驱动的 pi agent。
//!
//! 用法：`cargo run -p demo-agent`
//!
//! - 用 deepseek-chat（需 `DEEPSEEK_API_KEY`）作为默认模型，回退到 gpt-4o-mini。
//! - 启动后提示 `> `，输入消息回车执行；输入 `exit` / `quit` 或 Ctrl-D 退出。
//! - 每次调用工具前提示 `Y/n` 审批；拒绝（`n`）仅阻止当前工具，turn 继续。
//! - 会话记录到内存 session，prompt 前检查上下文 token 阈值，超限时自动 compaction。

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use pi_agent::harness::compaction::compaction::{
    CompactResult, DEFAULT_COMPACTION_SETTINGS, compact, estimate_context_tokens,
    prepare_compaction, should_compact,
};
use pi_agent::harness::session::context::build_session_context;
use pi_agent::harness::session::types::{CompactionEntry, Entry, EntryBase, MessageEntry};
use pi_agent::harness::tools::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool,
};
use pi_agent::harness::{ExecutionEnv, NodeExecutionEnv};
use pi_agent::{
    Agent, AgentEvent, AgentMessage, AgentOptions, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallResult, ToolExecutionMode,
};
use pi_ai::{AssistantMessageEvent, create_models, deepseek_provider, openai_provider, uuidv7};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // 1. LLM：注册 provider，构造 stream fn 和默认模型。
    let models = Arc::new(create_models());
    models.set_provider(openai_provider());
    models.set_provider(deepseek_provider());

    let model = models
        .get_model("deepseek", "deepseek-chat")
        .or_else(|| models.get_model("openai", "gpt-4o-mini"))
        .expect("未找到可用模型（deepseek-chat / gpt-4o-mini）");

    let stream_fn: pi_agent::StreamFn = {
        let models = models.clone();
        Arc::new(move |model, context, options| models.stream_simple(model, context, options))
    };

    // 2. 工具：bash / read / write / edit，基于当前目录。
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let env: Arc<dyn ExecutionEnv> = Arc::new(NodeExecutionEnv::new(cwd));
    let tools = vec![
        create_bash_tool(env.clone()),
        create_read_tool(env.clone()),
        create_write_tool(env.clone()),
        create_edit_tool(env.clone()),
    ];

    // 3. 审批门：每次工具调用前读 Y/n；拒绝仅阻止当前工具，不终止 turn。
    let before_tool_call = make_approval_gate();

    // 4. 内存 session 日志：记录会话 entries，用于 compaction。
    let session_log = Arc::new(SessionLog::new());

    // 5. 构造 agent。
    let agent = Agent::new(AgentOptions {
        system_prompt: Some(
            "You are a helpful coding assistant. Use the provided tools to inspect files \
             and run commands to accomplish the user's request. Work step by step."
                .to_string(),
        ),
        model: Some(model.clone()),
        tools: Some(tools),
        stream_fn: Some(stream_fn),
        before_tool_call: Some(before_tool_call),
        tool_execution: Some(ToolExecutionMode::Sequential),
        ..Default::default()
    });

    // 6. 订阅事件：实时打印过程，并把 assistant/tool 消息同步到 session。
    let session_log_events = session_log.clone();
    agent.subscribe(move |event, _signal| {
        let session_log = session_log_events.clone();
        Box::pin(async move {
            match event {
                AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                    ..
                } => {
                    print!("{delta}");
                    std::io::stdout().flush().ok();
                }
                AgentEvent::MessageEnd { message } => {
                    if let AgentMessage::Assistant(a) = &message
                        && let Some(err) = &a.error_message
                    {
                        println!();
                        println!("[错误] {err}");
                    }
                    if matches!(
                        message,
                        AgentMessage::Assistant(_) | AgentMessage::ToolResult(_)
                    ) {
                        session_log.append_message(message.clone());
                    }
                }
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    result,
                    is_error,
                    ..
                } => {
                    println!();
                    println!("[工具] {tool_name} 完成 (error={is_error})");
                    let pretty = serde_json::to_string_pretty(&result).unwrap_or_default();
                    if !pretty.is_empty() && pretty != "null" {
                        println!("[结果] {pretty}");
                    }
                }
                _ => {}
            }
        })
    });

    println!("demo-agent：输入消息执行；工具调用需审批；输入 exit 退出。");
    println!("模型: {}", agent.state().model.id);

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        let n = stdin.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            println!();
            break;
        }
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if matches!(input.as_str(), "exit" | "quit") {
            break;
        }

        // 7. prompt 前检查上下文阈值，超限则自动 compaction。
        maybe_compact(&agent, &session_log, &models, &model).await;

        // 8. 记录用户消息并执行。
        session_log.append_message(AgentMessage::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text(input.clone()),
            timestamp: pi_ai::utils::uuid::now_ms() as u64,
        }));
        agent.prompt_text(&input).await;
        agent.wait_for_idle().await;
        println!();
    }

    println!("再见。");
}

/// prompt 前检查上下文 token 阈值，超限则执行 compaction 并更新 agent 上下文。
async fn maybe_compact(
    agent: &Agent,
    session_log: &Arc<SessionLog>,
    models: &Arc<pi_ai::Models>,
    model: &pi_ai::Model,
) {
    let branch = session_log.get_branch();
    let session_ctx = build_session_context(&branch);
    let tokens = estimate_context_tokens(&session_ctx.messages).tokens;

    if !should_compact(tokens, model.context_window, &DEFAULT_COMPACTION_SETTINGS) {
        return;
    }

    let preparation = match prepare_compaction(&branch, &DEFAULT_COMPACTION_SETTINGS) {
        Ok(Some(prep)) => prep,
        _ => return,
    };

    println!();
    println!("[compaction] 上下文约 {tokens} tokens，超过阈值，开始压缩…");
    match compact(
        &preparation,
        models.as_ref(),
        model,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    {
        Ok(result) => {
            session_log.append_compaction(result);
            let new_branch = session_log.get_branch();
            let new_ctx = build_session_context(&new_branch);
            let after = estimate_context_tokens(&new_ctx.messages).tokens;
            agent.set_messages(new_ctx.messages);
            println!("[compaction] 完成，压缩后约 {after} tokens。");
        }
        Err(err) => {
            println!("[compaction] 失败: {err}");
        }
    }
}

/// 内存 session 日志：append-only 的 entry 链，支持从 leaf 回溯路径。
struct SessionLog {
    inner: Mutex<SessionLogInner>,
}

struct SessionLogInner {
    leaf_id: Option<String>,
    by_id: HashMap<String, Entry>,
    seq: u64,
}

impl SessionLog {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionLogInner {
                leaf_id: None,
                by_id: HashMap::new(),
                seq: 0,
            }),
        }
    }

    fn append_message(&self, message: AgentMessage) {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let id = uuidv7();
        let entry = Entry::Message(MessageEntry {
            base: EntryBase {
                kind: "message".to_string(),
                id: id.clone(),
                seq: inner.seq,
                parent_id: inner.leaf_id.clone(),
                timestamp: pi_ai::utils::uuid::now_ms() as u64,
            },
            message,
            terminate: None,
        });
        inner.leaf_id = Some(id.clone());
        inner.by_id.insert(id, entry);
    }

    fn append_compaction(&self, result: CompactResult) {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let id = uuidv7();
        let details = serde_json::to_value(&result.details).ok();
        let entry = Entry::Compaction(CompactionEntry {
            base: EntryBase {
                kind: "compaction".to_string(),
                id: id.clone(),
                seq: inner.seq,
                parent_id: inner.leaf_id.clone(),
                timestamp: pi_ai::utils::uuid::now_ms() as u64,
            },
            summary: result.summary,
            retained_tail: result.retained_tail,
            tokens_before: result.tokens_before,
            details,
            usage: result.usage,
        });
        inner.leaf_id = Some(id.clone());
        inner.by_id.insert(id, entry);
    }

    /// 从 leaf 沿 parent_id 回溯到 root，返回路径（root → leaf 顺序）。
    fn get_branch(&self) -> Vec<Entry> {
        let inner = self.inner.lock().unwrap();
        let mut path = Vec::new();
        let mut current = inner.leaf_id.clone();
        while let Some(id) = current {
            let Some(entry) = inner.by_id.get(&id) else {
                break;
            };
            current = entry_parent_id(entry);
            path.push(entry.clone());
        }
        path.reverse();
        path
    }
}

fn entry_parent_id(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Message(e) => e.base.parent_id.clone(),
        Entry::ModelChange(e) => e.base.parent_id.clone(),
        Entry::ThinkingLevelChange(e) => e.base.parent_id.clone(),
        Entry::ActiveToolsChange(e) => e.base.parent_id.clone(),
        Entry::Compaction(e) => e.base.parent_id.clone(),
        Entry::BranchSummary(e) => e.base.parent_id.clone(),
        Entry::Custom(e) => e.base.parent_id.clone(),
    }
}

/// 构造审批门回调：每个工具调用前提示 Y/n；拒绝仅阻止当前工具，不终止 turn。
fn make_approval_gate() -> BeforeToolCallFn {
    Arc::new(move |ctx: &BeforeToolCallContext, _signal| {
        let tool_name = ctx.tool_call.name.clone();
        let args = ctx.args.clone();
        Box::pin(async move {
            println!();
            println!("┌─ 工具审批 ────────────────────────");
            println!("│ 工具: {tool_name}");
            let args = serde_json::to_string_pretty(&args).unwrap_or_default();
            for line in args.lines() {
                println!("│   {line}");
            }
            println!("└──────────────────────────────────");

            loop {
                print!("批准执行? [Y/n]: ");
                std::io::stdout().flush().ok();

                let mut line = String::new();
                let n = std::io::stdin().read_line(&mut line).unwrap_or(0);
                if n == 0 {
                    println!("(stdin 关闭，视为拒绝)");
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("stdin closed".to_string()),
                        terminate: false,
                    });
                }

                match line.trim().to_ascii_lowercase().as_str() {
                    "" | "y" | "yes" => return None,
                    "n" | "no" => {
                        println!("已拒绝，继续后续工具。");
                        return Some(BeforeToolCallResult {
                            block: true,
                            reason: Some("用户拒绝".to_string()),
                            terminate: false,
                        });
                    }
                    _ => println!("请输入 Y 或 N"),
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::harness::compaction::compaction::CompactionDetails;

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    #[test]
    fn session_log_branch_follows_parent_chain() {
        let log = SessionLog::new();
        log.append_message(user_msg("a"));
        log.append_message(user_msg("b"));

        let branch = log.get_branch();
        assert_eq!(branch.len(), 2);
        // root → leaf 顺序，且 parent 链正确。
        assert!(matches!(&branch[0], Entry::Message(e) if e.base.parent_id.is_none()));
        assert!(
            matches!(&branch[1], Entry::Message(e) if e.base.parent_id.as_deref() == Some(branch[0].id()))
        );
    }

    #[test]
    fn compaction_entry_truncates_context() {
        let log = SessionLog::new();
        log.append_message(user_msg("old"));
        log.append_message(user_msg("recent"));
        log.append_compaction(CompactResult {
            summary: "summary".to_string(),
            tokens_before: 100,
            usage: None,
            retained_tail: vec![user_msg("recent")],
            details: CompactionDetails {
                read_files: vec![],
                modified_files: vec![],
            },
        });

        let branch = log.get_branch();
        let ctx = build_session_context(&branch);
        // 只保留最后一个 compaction 展开：summary + retained_tail，旧历史被截断。
        assert_eq!(ctx.messages.len(), 2);
        assert!(matches!(
            &ctx.messages[0],
            AgentMessage::CompactionSummary(_)
        ));
    }
}
