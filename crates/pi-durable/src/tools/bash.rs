//! 对应 `tools/bash.ts`：通过环境 Shell 运行命令。

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use serde_json::{Value as JsonValue, json};

use crate::chord::context::Context;
use crate::env::{
    ExecutionEnv, ExecutionErrorCode, ShellCommand, ShellExecOptions, ShellExecResult,
    ShellOutputInfo, ShellSpillOptions,
};
use crate::harness::define::define_tool;
use crate::harness::output::{OutputChunk, OutputLimits, Retain};
use crate::harness::types::{
    ToolDiagnostic, ToolDiagnosticSeverity, ToolExecutionApi, ToolExecutionResult, ToolRegistration,
};
use crate::session::SessionError;
use crate::tools::env::require_env;
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;

static SCHEMA: LazyLock<JsonValue> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
        },
        "required": ["command"]
    })
});

/// 对应 `BashPrepare`：命令执行前修改 execution（脚本/工作目录/环境）；抛错（Rust 为 Err）会中止执行。
pub type BashPrepare = Arc<
    dyn Fn(
            &mut BashExecution,
            &dyn ToolExecutionApi,
            Arc<dyn Context>,
        ) -> BoxFuture<'static, Result<(), SessionError>>
        + Send
        + Sync,
>;

/// 对应 `BashToolOptions`。
#[derive(Clone, Default)]
pub struct BashToolOptions {
    /// 在命令前运行的行。
    pub command_prefix: Option<String>,
    /// 执行前的修改钩子。
    pub prepare: Option<BashPrepare>,
}

/// 对应 `PowerShellToolOptions`。
#[derive(Clone, Default)]
pub struct PowerShellToolOptions {
    /// 在命令前运行的行。
    pub command_prefix: Option<String>,
    /// 执行前的修改钩子。
    pub prepare: Option<BashPrepare>,
    /// PowerShell 程序按序尝试；默认 `pwsh` 再 `powershell`。
    pub programs: Option<Vec<String>>,
}

struct BashTool {
    options: BashToolOptions,
}

/// 对应 `BashExecution`：一条即将运行的命令，`prepare` 可修改。
pub struct BashExecution {
    pub command: String,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
}

fn validate_timeout(timeout: Option<f64>) -> Result<(), SessionError> {
    let Some(timeout) = timeout else {
        return Ok(());
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(SessionError::Message(
            "Invalid timeout: must be a finite number of seconds".to_string(),
        ));
    }
    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(SessionError::Message(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        )));
    }
    Ok(())
}

/// 对应 `prepareExecution`：构造 execution（前缀 + cwd + 空 env + 继承），再跑 `prepare` 钩子。
async fn prepare_execution(
    command: &str,
    command_prefix: Option<&str>,
    prepare: Option<&BashPrepare>,
    api: &dyn ToolExecutionApi,
    context: Arc<dyn Context>,
) -> Result<BashExecution, SessionError> {
    let env = require_env(api)?;
    let mut execution = BashExecution {
        command: command_prefix.map_or_else(
            || command.to_string(),
            |prefix| format!("{prefix}\n{command}"),
        ),
        cwd: env.cwd(),
        env: BTreeMap::new(),
        inherit_env: true,
    };
    if let Some(prepare) = prepare {
        prepare(&mut execution, api, context).await?;
    }
    Ok(execution)
}

/// 对应 `runCommand`：依次尝试命令形态直到有一个启动，流式输出并在非零退出/超时时报错。
async fn run_command(
    commands: Vec<ShellCommand>,
    execution: &BashExecution,
    timeout: Option<f64>,
    api: &Arc<dyn ToolExecutionApi>,
    env: &dyn ExecutionEnv,
    context: Arc<dyn Context>,
) -> Result<(), SessionError> {
    let mut result: Option<Result<ShellExecResult, crate::env::ExecutionError>> = None;
    for command in commands {
        let on_output_api = Arc::clone(api);
        let outcome = env
            .exec(
                command,
                ShellExecOptions {
                    cwd: Some(execution.cwd.clone()),
                    env: Some(execution.env.clone()),
                    inherit_env: Some(execution.inherit_env),
                    timeout,
                    on_output: Some(Box::new(
                        move |text: &str, _context: &dyn Context, info: &ShellOutputInfo| {
                            on_output_api.output(OutputChunk::Text(text), info.skipped);
                        },
                    )),
                    spill: Some(ShellSpillOptions {
                        after_bytes: DEFAULT_MAX_BYTES,
                        after_lines: DEFAULT_MAX_LINES,
                    }),
                    window: api.output_window(),
                    ..Default::default()
                },
                context.as_ref(),
            )
            .await;
        let spawn_error = matches!(
            outcome,
            Err(ref error) if error.code == ExecutionErrorCode::SpawnError
        );
        let is_last = false;
        result = Some(outcome);
        if !spawn_error {
            break;
        }
        let _ = is_last;
    }
    let Some(result) = result else {
        return Err(SessionError::Message("No command to run".to_string()));
    };
    let spill_path = match &result {
        Ok(value) => value.spill_path.clone(),
        Err(error) => error.spill_path.clone(),
    };
    if let Some(spill_path) = spill_path {
        api.diagnostic(ToolDiagnostic {
            severity: ToolDiagnosticSeverity::Info,
            code: Some("full_output".to_string()),
            message: format!("Full output: {spill_path}"),
        });
    }
    match result {
        Err(error) => {
            if error.code == ExecutionErrorCode::Aborted
                && context.abort_signal().is_some_and(|s| s.aborted())
            {
                return Err(SessionError::from(error));
            }
            if error.code == ExecutionErrorCode::Timeout {
                return Err(SessionError::Message(format!(
                    "Command timed out after {} seconds",
                    timeout.unwrap_or_default()
                )));
            }
            if error.code == ExecutionErrorCode::Aborted {
                return Err(SessionError::Message("Command aborted".to_string()));
            }
            Err(SessionError::from(error))
        }
        Ok(value) => {
            if value.exit_code != 0 {
                Err(SessionError::Message(format!(
                    "Command exited with code {}",
                    value.exit_code
                )))
            } else {
                Ok(())
            }
        }
    }
}

#[async_trait::async_trait]
impl ToolRegistration for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns combined stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
    }

    fn parameters(&self) -> &JsonValue {
        &SCHEMA
    }

    fn output_limits(&self) -> Option<OutputLimits> {
        Some(OutputLimits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            retain: Retain::Tail,
        })
    }

    async fn execute(
        &self,
        args: JsonValue,
        api: Arc<dyn ToolExecutionApi>,
        context: Arc<dyn Context>,
    ) -> Result<ToolExecutionResult, SessionError> {
        let command = args["command"].as_str().unwrap_or_default();
        let timeout = args["timeout"].as_f64();
        validate_timeout(timeout)?;
        let execution = prepare_execution(
            command,
            self.options.command_prefix.as_deref(),
            self.options.prepare.as_ref(),
            api.as_ref(),
            Arc::clone(&context),
        )
        .await?;
        let env = require_env(api.as_ref())?;
        run_command(
            vec![ShellCommand::Shell(execution.command.clone())],
            &execution,
            timeout,
            &api,
            env.as_ref(),
            Arc::clone(&context),
        )
        .await?;
        Ok(ToolExecutionResult::default())
    }
}

/// 对应 `createBashTool(options?)`。
pub fn create_bash_tool(options: Option<BashToolOptions>) -> Arc<dyn ToolRegistration> {
    define_tool(BashTool {
        options: options.unwrap_or_default(),
    })
}

/// PowerShell 启动参数（对应 `POWERSHELL_ARGS`）。
const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];
const UTF8_OUTPUT: &str = "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}";

/// 对应 `createPowerShellTool(options?)`。
pub fn create_powershell_tool(options: Option<PowerShellToolOptions>) -> Arc<dyn ToolRegistration> {
    let options = options.unwrap_or_default();
    struct PowerShellTool {
        options: PowerShellToolOptions,
    }
    #[async_trait::async_trait]
    impl ToolRegistration for PowerShellTool {
        fn name(&self) -> &str {
            "powershell"
        }

        fn description(&self) -> &str {
            "Execute a PowerShell command in the current working directory. Returns combined stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
        }

        fn parameters(&self) -> &JsonValue {
            &SCHEMA
        }

        fn output_limits(&self) -> Option<OutputLimits> {
            Some(OutputLimits {
                max_bytes: DEFAULT_MAX_BYTES,
                max_lines: DEFAULT_MAX_LINES,
                retain: Retain::Tail,
            })
        }

        async fn execute(
            &self,
            args: JsonValue,
            api: Arc<dyn ToolExecutionApi>,
            context: Arc<dyn Context>,
        ) -> Result<ToolExecutionResult, SessionError> {
            let command = args["command"].as_str().unwrap_or_default();
            let timeout = args["timeout"].as_f64();
            validate_timeout(timeout)?;
            let execution = prepare_execution(
                command,
                self.options.command_prefix.as_deref(),
                self.options.prepare.as_ref(),
                api.as_ref(),
                Arc::clone(&context),
            )
            .await?;
            let env = require_env(api.as_ref())?;
            let script = format!("{UTF8_OUTPUT}\n{}", execution.command);
            let default_programs = ["pwsh".to_string(), "powershell".to_string()];
            let programs = self
                .options
                .programs
                .as_deref()
                .unwrap_or(&default_programs);
            let commands = programs
                .iter()
                .map(|program| {
                    let mut argv = vec![program.clone()];
                    argv.extend(POWERSHELL_ARGS.iter().map(|arg| (*arg).to_string()));
                    argv.push(script.clone());
                    ShellCommand::Argv(argv)
                })
                .collect();
            run_command(commands, &execution, timeout, &api, env.as_ref(), context).await?;
            Ok(ToolExecutionResult::default())
        }
    }
    define_tool(PowerShellTool { options })
}
