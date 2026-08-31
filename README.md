# pi_rs

[Rust](https://www.rust-lang.org/) 复刻的 Pi agent 基座 —— 从 [earendil-works/pi](https://github.com/earendil-works/pi)（TypeScript）逐模块 1:1 移植的通用 agent 运行时。

项目目标：学习 Pi 的核心 agent 架构，并用 Rust 重写一个可运行的 agent 基座（能接 LLM、跑 agent loop、读写文件/执行命令、会话持久化、上下文压缩）。

## 模块结构

```
pi-agent   agent 运行时（Agent 类 + 双层循环 + 工具执行 + session 持久化 + compaction）
  ├── pi-ai        统一 LLM API（消息/流/工具校验 + openai/deepseek provider）
  └── pi-telemetry vendor 中立 telemetry 契约（span/event 记录）
```

| crate | 复刻自 | 文件 | 说明 |
|---|---|---|---|
| [`pi-telemetry`](crates/pi-telemetry/AGENT.md) | `packages/telemetry` | 6/6 完整 | span 生命周期、内存/NOOP 后端、conformance 契约 |
| [`pi-ai`](crates/pi-ai/AGENT.md) | `packages/ai` | 核心子集 | 类型 + 流抽象 + 工具校验 + openai/deepseek/faux |
| [`pi-agent`](crates/pi-agent/AGENT.md) | `packages/agent` | 50/50 完整 | Agent 核心 + 全部 harness（工具/session/compaction/skills） |

每个 crate 目录下的 `AGENT.md` 记录了复刻过程、与原版的差异、模块原理与阅读步骤。

## 构建与测试

要求：Rust 1.85+（edition 2024）。

```bash
cargo build                 # 编译 workspace
cargo test                  # 运行全部测试（26 个）
cargo clippy --all-targets  # lint（0 警告）
cargo fmt --all -- --check  # 格式检查
```

## 复刻范围与差异

- **pi-telemetry**、**pi-agent**：文件级 1:1 复刻。方法级差异仅为 Rust 语言固有限制（类型体操、`AsyncIterable` → `Vec`）和「依赖外部 LLM 服务」的少数路径（如 `generateBranchSummary` 的实际 LLM 调用）。
- **pi-ai**：按「provider 不需要实现所有，但要有 openai 和 deepseek」的要求翻译核心子集（16/177 文件）。省略 40+ provider 的 HTTP 实现、OAuth 认证、图像生成、模型目录等。

各模块的完整差异清单见对应 `AGENT.md`。

## License

MIT（与原项目 Pi 一致）。
