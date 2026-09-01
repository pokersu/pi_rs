//! demo-agent：交互式 REPL 驱动的 pi agent。
//!
//! 用法：`cargo run -p demo-agent`
//!
//! - 用 deepseek-chat（需 `DEEPSEEK_API_KEY`）作为默认模型，回退到 gpt-4o-mini。
//! - 启动后提示 `> `，输入消息回车执行；输入 `exit` / `quit` 或 Ctrl-D 退出。
//! - 每次调用工具前提示 `Y/n` 审批；拒绝（`n`）则结束本轮工具执行并回到 `> `。

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_agent::harness::tools::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool,
};
use pi_agent::harness::{ExecutionEnv, NodeExecutionEnv};
use pi_agent::{
    Agent, AgentEvent, AgentMessage, AgentOptions, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallResult, ToolExecutionMode,
};
use pi_ai::{AssistantMessageEvent, create_models, deepseek_provider, openai_provider};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // 1. LLM：注册 provider，构造 stream fn 和默认模型。
    let models = create_models();
    models.set_provider(openai_provider());
    models.set_provider(deepseek_provider());

    let model = models
        .get_model("deepseek", "deepseek-chat")
        .or_else(|| models.get_model("openai", "gpt-4o-mini"))
        .expect("未找到可用模型（deepseek-chat / gpt-4o-mini）");

    let stream_fn: pi_agent::StreamFn =
        Arc::new(move |model, context, options| models.stream_simple(model, context, options));

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

    // 3. 审批门：每次工具调用前读 Y/n。拒绝后终止本轮后续工具。
    let rejected = Arc::new(AtomicBool::new(false));
    let before_tool_call = make_approval_gate(rejected.clone());

    // 4. 构造 agent。
    let agent = Agent::new(AgentOptions {
        system_prompt: Some(
            "You are a helpful coding assistant. Use the provided tools to inspect files \
             and run commands to accomplish the user's request. Work step by step."
                .to_string(),
        ),
        model: Some(model),
        tools: Some(tools),
        stream_fn: Some(stream_fn),
        before_tool_call: Some(before_tool_call),
        tool_execution: Some(ToolExecutionMode::Sequential),
        ..Default::default()
    });

    // 5. 订阅事件，实时打印过程。
    agent.subscribe(|event, _signal| {
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
        // 每轮开始前重置拒绝标志。
        rejected.store(false, Ordering::SeqCst);

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

        agent.prompt_text(&input).await;
        agent.wait_for_idle().await;
        println!();
    }

    println!("再见。");
}

/// 构造审批门回调：每个工具调用前提示 Y/n。
fn make_approval_gate(rejected: Arc<AtomicBool>) -> BeforeToolCallFn {
    Arc::new(move |ctx: &BeforeToolCallContext, _signal| {
        let rejected = rejected.clone();
        let tool_name = ctx.tool_call.name.clone();
        let args = ctx.args.clone();
        Box::pin(async move {
            if rejected.load(Ordering::SeqCst) {
                return Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("已拒绝，停止后续工具".to_string()),
                    terminate: true,
                });
            }

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
                    rejected.store(true, Ordering::SeqCst);
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("stdin closed".to_string()),
                        terminate: true,
                    });
                }

                match line.trim().to_ascii_lowercase().as_str() {
                    "" | "y" | "yes" => return None,
                    "n" | "no" => {
                        rejected.store(true, Ordering::SeqCst);
                        println!("已拒绝，结束本轮工具执行。");
                        return Some(BeforeToolCallResult {
                            block: true,
                            reason: Some("用户拒绝".to_string()),
                            terminate: true,
                        });
                    }
                    _ => println!("请输入 Y 或 N"),
                }
            }
        })
    })
}
