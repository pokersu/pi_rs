//! demo-agent：基于 `pi-durable` harness 的命令行演示 agent。
//!
//! 用法：`cargo run -p demo-agent`
//!
//! - 用 deepseek-chat（需 `DEEPSEEK_API_KEY`）作为默认模型，回退到 gpt-4o-mini。
//! - 启动后提示 `> `，输入消息回车执行；输入 `exit` / `quit` 或 Ctrl-D 退出。
//! - 工具为 `coding-tools`（read / write / edit / bash），运行在真实文件系统的当前目录。

use std::io::{self, Write};
use std::sync::Arc;

use pi_ai::{create_models, deepseek_provider, openai_provider};
use pi_durable::chord::context::{Context, EmptyContext};
use pi_durable::env::NodeExecutionEnv;
use pi_durable::harness::harness::open;
use pi_durable::harness::registry::create_registry;
use pi_durable::harness::types::{
    AgentChange, EnvBuilder, ExtensionChangeSelection, FieldChange, Harness, HarnessOptions,
    InputSubmissionDraft, ModelRef, RegistryReader, SubmissionDraft, WhenBusy,
};
use pi_durable::session::Session;
use pi_durable::storage::MemoryStorage;
use pi_durable::tools::CODING_TOOLS;
use pi_durable::types::Storage;

fn context() -> Arc<dyn Context> {
    Arc::new(EmptyContext::new("[demo-agent]"))
}

fn last_assistant_text(messages: &[pi_ai::Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pi_ai::Message::Assistant(assistant) => {
                let text: String = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pi_ai::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect();
                Some(text)
            }
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // 1. 模型目录与 provider。
    let models = Arc::new(create_models());
    models.set_provider(openai_provider());
    models.set_provider(deepseek_provider());
    let model = models
        .get_model("deepseek", "deepseek-chat")
        .or_else(|| models.get_model("openai", "gpt-4o-mini"))
        .expect("未找到可用模型（deepseek-chat / gpt-4o-mini）");
    let (provider, model_id) = (model.provider.clone(), model.id.clone());

    // 2. 存储、注册表与内置工具。
    let storage = Arc::new(MemoryStorage::new());
    let registry = create_registry();
    registry.install(Arc::new(CODING_TOOLS.clone()));

    // 3. 环境：每个会话用其 `cwd` 构建真实文件系统 + Shell。
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let env: EnvBuilder = Arc::new(move |target, _ctx| {
        let cwd = target.cwd.clone().unwrap_or_else(|| cwd.clone());
        let env = Arc::new(NodeExecutionEnv::new(cwd));
        Box::pin(async move { Ok(Some(env as Arc<dyn pi_durable::env::ExecutionEnv>)) })
    });

    // 4. 打开 Harness。
    let harness = open(
        storage as Arc<dyn Storage>,
        HarnessOptions {
            models,
            registry: registry as Arc<dyn RegistryReader>,
            settings: None,
            env: Some(env),
            conversation_created: None,
            now: None,
            on_report: Some(Arc::new(|error| eprintln!("[上报] {error}"))),
        },
        context(),
    )
    .await
    .expect("open harness");

    // 5. 根会话：模型 + coding-tools。
    let change = AgentChange {
        model: FieldChange::Set(ModelRef { provider, model_id }),
        extensions: FieldChange::Set(ExtensionChangeSelection::Exactly(vec![Arc::new(
            CODING_TOOLS.clone(),
        )])),
        ..Default::default()
    };
    let conversation = harness
        .root(context(), Some(change), None)
        .await
        .expect("root conversation");

    println!("demo-agent 已就绪（输入 exit / quit 退出）");

    // 6. REPL。
    loop {
        print!("> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }

        let submission = conversation
            .submit(
                SubmissionDraft::Input(InputSubmissionDraft {
                    request_id: None,
                    content: pi_ai::UserContent::Text(line),
                    when_busy: Some(WhenBusy::Reject),
                }),
                context(),
            )
            .await
            .expect("submit");
        let settled = submission.wait(context()).await.expect("wait");

        // 从 transcript 读最后的 assistant 回答。
        let view = conversation
            .context(context(), None)
            .await
            .expect("context");
        let answer = last_assistant_text(&view.messages);
        if answer.is_empty() {
            println!(
                "[状态] {}",
                serde_json::to_string(&settled.record().status()).unwrap_or_default()
            );
        } else {
            println!("{answer}");
        }
    }

    harness.close(context()).await.ok();
}
