# pi_rs

[Rust](https://www.rust-lang.org/) 复刻的 Pi agent 基座 —— 从 [earendil-works/pi](https://github.com/earendil-works/pi)（TypeScript）逐模块 1:1 移植的通用 agent 运行时。

项目目标：学习 Pi 的核心 agent 架构，并用 Rust 重写一个可运行的 agent 基座（能接 LLM、跑 agent loop、读写文件/执行命令、会话持久化、上下文压缩）。

## 模块结构

```
pi-agent-core   agent 核心（Agent 类 + 双层循环 + proxy/stream-fn，v1.1.0 起不再含 harness）
pi-durable     durable conversation / task / document 运行时（上游 v1.0.0 从 agent 包移出的能力）
  ├── pi-ai        统一 LLM API（消息/流/工具校验 + openai/deepseek provider）
  └── pi-telemetry vendor 中立 telemetry 契约（span/event 记录）
```

> ✅ **架构换代已完成**（v0.99.2 → v1.1.0）。上游 v1.0.0 把 harness（sessions、storage、tools、
> compaction、skills…）从 `pi-agent` 移除并重写为 `pi-durable`，新架构是 document/transaction +
> task-graph，旧 lane/drive 状态机已不复存在。因此旧 harness 的 Rust 实现已删除（见 git 历史），
> `pi-durable` 已按新架构完整重建（P0–P10）：chord 子集、基础层、storage 三后端、env、session、
> harness（task-graph 调度器 + 内置 Task + registry）、tools、testing 全部落地。
> 计划与进度见 `UPSTREAM-SYNC-v1.1.0.md`；双向审计结论见 `tools.d/parity/AUDIT-REPORT.md`。

| crate | 复刻自 | 状态 | 说明 |
|---|---|---|---|
| [`pi-telemetry`](crates/pi-telemetry/AGENT.md) | `packages/telemetry` | 6/6 完整 | span 生命周期、内存/NOOP 后端、conformance 契约 |
| [`pi-ai`](crates/pi-ai/AGENT.md) | `packages/ai` | 44/178 核心子集 | 类型 + 流抽象 + 工具校验 + auth 框架 + openai/deepseek/faux |
| [`pi-agent-core`](crates/pi-agent-core/) | `packages/agent` | 6/6 完整 | Agent 类、agent loop、proxy、stream-fn、类型 |
| `pi-durable` | `packages/durable` | 67/67 完整 | document/task/storage（memory / jsonl / sqlite）+ harness + tools + testing |

每个 crate 目录下的 `AGENT.md` 记录了复刻过程、与原版的差异、模块原理与阅读步骤。

## 构建与测试

要求：Rust 1.85+（edition 2024）。

```bash
cargo build                 # 编译 workspace
cargo test                  # 运行全部测试（470 个）
cargo clippy --all-targets  # lint（pi-ai/pi-agent-core/pi-telemetry 0 告警；pi-durable 有 7 处历史告警）
cargo fmt --all -- --check  # 格式检查
```

## 复刻范围与差异

- **pi-telemetry**：文件级 1:1 复刻，无差异。
- **pi-agent-core**：对应上游 `packages/agent`（v1.1.0 的 6 个文件），已对齐；agent 层自定义消息的转换已按上游改为 `defaultConvertToLlm` 的过滤语义。
- **pi-durable**：已按上游 `packages/durable`（v1.1.0，67 文件）完整复刻。已落地 chord 子集（含
  `MutableReplicatedState`/`ReplicatedStateReplica`/`state-codec` 的 wire 编解码）、
  顶层类型/文档/条目/任务基础层、`storage` 三后端（memory / jsonl / sqlite）、`env` 契约 + node 实现、
  `session`、`harness`（task-graph 调度器 + 内置 Task + registry）、`tools`、`testing`；
  语言机制导致的差异（sqlite 的异步 facade 豁免、JSON 列 schema 等）见 `UPSTREAM-SYNC-v1.1.0.md`。
- **pi-ai**：按「provider 不需要实现所有，但要有 openai 和 deepseek」的要求翻译核心子集（44/178 文件）。省略 40+ provider 的 HTTP 实现、OAuth 登录流程、图像生成、模型目录等。

各模块的完整差异清单见对应 `AGENT.md`。

## 上游对照验证

纯逻辑模块（如 `harness/output.rs`）用「直接加载上游 TypeScript 计算期望值」的方式做 1:1 校验：

```bash
node --experimental-strip-types tools.d/parity/output-parity.mjs   # 生成用例与期望值
cargo test -p pi-durable --test output_parity                      # 重放并逐项比对
```

需要 Node ≥ 22.6（`--experimental-strip-types`）。crate 本身不依赖 Node。

## License

MIT（与原项目 Pi 一致）。
