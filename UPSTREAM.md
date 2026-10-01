# 上游同步基线（Pi）

> 用途：记录 pi_rs 与上游 Pi 的同步位置，避免进度丢失。
> **每次同步后请更新本文件。**

## 当前状态

- **已同步到上游 tag**：`v0.99.2`（2026-09-30）
- **同步日期**：2026-10-01
- **本次同步范围**：`f3564a1d..v0.99.2`（312 个 commit / 12 个 tag），按 `UPSTREAM-SYNC-v0.99.2.md` 的 P1–P7 分阶段移植
- **本地提交链**：`d07a2ab`(P1) → `bce7b0b`(P2) → `e3c01c4`(P3a) → `080a062`(P3b) → P4/P5 提交
- **已知偏差（本次未同步）**：
  - `onProviderStreamEvent`（0.99.0）未实现 —— 需穿透 provider 流层（`StreamOptions` + 各 provider 的事件解析处），属独立改动
  - overflow 的 Z.AI CN 端点检测（可选，本项目未使用 Z.AI）
  - HTTP-date 形式的 `Retry-After` 仍为简化实现（上游用 `Date.parse`，Rust 侧忽略该 header 走指数退避）

### 上一轮（历史）

- **曾同步到**：`f3564a1d`（= `v0.85.1-35-gf3564a1d4`），2026-09-10，本地提交 `9cdb3b5`
  - fork 重构 / JSONL 两阶段流式 fork / retry 退避上限 / `open_text_line_reader`
  - 另有 harness fault 路径修复（`af4136f`）

## 待同步（已识别，尚未移植）

- **同步目标（tag 锚点）**：`v0.99.2`（2026-09-30，最新稳定 tag）
- **上游 main HEAD（仅供参考）**：`8ce69e9d2`（= `v0.99.2` + 17 个未发版 commit）
- **当前基线**：`f3564a1d` = `v0.85.1-35-gf3564a1d4`（即 v0.85.1 之后 35 个 commit，**未发版的中间状态**）
- **待同步范围**：`f3564a1d..v0.99.2` — **312 个 commit**，跨 **12 个 tag**（2026-09-05 → 2026-09-30）
- **改动规模**：`packages/agent` 95 文件 +34,840/-382（含实验性 pico3 8,074 行）；`packages/ai` 240 文件 +12,296/-4,887；`packages/telemetry` 2 文件 +17/-3
- **主代码（排除 pico3）**：agent 侧仅 9 文件 +383/-174

### 进度

- ✅ **P1 消息模型扩展**（2026-10-01 完成）：`Message::System` / `SystemMessage` / `SystemContent` / `ToolReference`；`AgentMessage::System`；text 渲染函数；estimate / transform_messages / 两个 provider 的 system 分支；新增 8 个测试
- ⬜ P2 transcript 工具层（`utils/transcript.rs` + `normalizeContext`）
- ✅ **P2 transcript 工具层**（2026-10-01 完成）：新增 `utils/transcript.rs`（17 个导出）+ `TranscriptContext` 类型 + `sections` 改为 `IndexMap`（保插入序）；新增 10 个测试。**stream 入口切换与 provider 消费并入 P3**。
- ✅ **P3a provider 侧接入 transcript**（2026-10-01 完成）：`openai-responses` / `openai-completions` 改为 `normalize_context` → `resolve_transcript` → `resolve_transcript_tools`；删除内联 `split_deferred_tools`，新增 `append_system_tool_additions`（非首条 system 的 `toolsAdded` → `additional_tools` / `tool_search`）；completions 引入 `instructionRole` 与 Kimi 风格 `system+tools`；`namespace` 回放改为只看 `is_same_model`（对齐上游）；`Compat` 新增 `supportsMidConvoSystemMessages`。3 个新测试。
- ✅ **P3b agent-loop 侧**（2026-10-01 完成）：`agent-loop.rs` 新增 `declare_tool_changes`（把可执行工具集与 transcript 声明之差写成 system 消息的 `toolsAdded`/`toolsRemoved`）+ `with_tool_changes` / `declared_tools` / `executable_tools`；`fold_initial_system_message` 把 `systemPrompt`+`tools` 折叠为首条 system 消息（字段清空）；在 `run_agent_loop` 入口与 `run_loop` 每轮 pending 注入前接入。`drive/tool-placement.rs` 移除 `activeToolNames` 自动增量写回与 `ConfigUpdate::ActiveTools` 事件（工具激活改为显式：`setActiveTools` / 调用方更新工具集，装载变化由 transcript 承载）。7 个新测试。
- ✅ **P4 循环钩子 Breaking**（2026-10-01 完成）：新增 `finishTurn`（返回 `AgentTurnDecision::End|Continue`，在 assistant+工具结果 finalize 后、`turn_end` 前运行，决策在 `turn_end` 后应用）取代 `shouldStopAfterTurn`；新增 `prepareRequest`（每次 provider 请求前，含首次，可替换 context/model/thinkingLevel）；新增 `Agent.peekQueuedMessages()`；`AgentTurnContext` 取代 `ShouldStopAfterTurnContext`（旧名保留为别名）。2 个新集成测试。
- ✅ **P5 小项**（2026-10-01 完成）：`AssistantMessage.thinkingLevel`（25 处构造点补齐，agent-loop 两处填充）；image 检测改为 `GIF87a`/`GIF89a`（避免文本文件误判）；provider-retry 对非有限 `Retry-After` 回落指数退避。**未做**：`onProviderStreamEvent`（需穿透 provider 流层，独立改动）、overflow Z.AI CN 检测（可选）。
- ✅ **P6 telemetry**（2026-10-01 确认无需代码改动）：上游仅改 CHANGELOG 与 package.json 版本号，源码零改动。
- ✅ **P7 验证与收尾**（2026-10-01 完成）：`cargo check` / `clippy`（0 告警）/ `test`（21 套件全绿）/ `fmt` 全部通过；AGENT.md 与方案文档已同步。
### 待同步清单（按优先级）

1. **〔重大〕Mid-conversation system messages**（commit `9e05370b2`，PR #9548）
   - 新增 `SystemMessage`（`role:"system"` + `content` + `sections` + **`toolsAdded`** + **`toolsRemoved`** + `timestamp`），位于 `packages/ai/src/types.ts`
   - 新增 `packages/ai/src/utils/transcript.ts`（`createInitialSystemMessage` 等，234 行）
   - **删除 `packages/ai/src/utils/deferred-tools.ts`**（工具增量激活改由 system message 承载）
   - agent 侧：`harness/messages.ts` 的 `convertToLlm` 新增 `case "system"`；`drive/tool-placement.ts` 移除 `activeToolNames` 增量写入（-44 行）
   - 影响 pi_rs：`pi-ai/types.rs`（Message 枚举）、`harness/messages.rs`（convert_to_llm）、`drive/tool-placement.rs`、`api/openai-responses.rs`（内联的 split_deferred_tools）、会话恢复/分支导航
2. **〔Breaking〕Agent 循环钩子变更**（0.86.0 / 0.87.0）
   - 移除 `shouldStopAfterTurn` → 新增 **`finishTurn`**（返回 `{action:"end"|"continue"}`，在 assistant/tool 结果 finalize 后、`turn_end` 前运行）
   - 新增 **`prepareRequest`**（每次 provider 请求前，含首次）
   - 新增 **`Agent.peekQueuedMessages()`**
   - `prepareNextTurn` / `prepareNextTurnWithContext` 语义调整（仅在确定开始下一 assistant turn 时运行）
   - 影响：`agent-loop.rs`、`agent.rs`、`types.rs`
3. **〔新增〕** `onProviderStreamEvent`（0.99.0）；assistant message 记录 `thinkingLevel`（0.99.0）
4. **〔修复〕** `harness/tools/image.ts`：以 `GIF` 开头的文本文件被误判为图片；`utils/provider-retry.ts`：`Retry-After` 不可解析时改用指数退避；`utils/overflow.ts`：Z.AI CN 端点溢出检测
5. **〔范围外，暂不搬〕** ai 的图像模型统一（`ImagesModels` 移除）、classifier 模型与 `Models.classify()`、模型目录新增（Claude Sonnet 5.5 / GPT-6.1 Sol / Grok 4.7 等）、OAuth 页面移至 `utils/oauth-page.ts`、Sign in with ChatGPT
6. **〔实验性，建议观察〕** `packages/agent/src/harness/pico3/`（25 文件 / 8,074 行）—— 明确标注 "Experimental Pico3 kernel API"，未进包根导出；`docs/pico-v3.md` 称 "Design under discussion"。待其稳定后再评估是否纳入复刻范围。
7. **〔小〕** `packages/telemetry` 2 文件 +17/-3

## 上游源码镜像（只读参考，已 gitignore）

```
tools.d/upstream-ref/
  agent-src/      ← packages/agent/src（92 个 TS）
  ai-src/         ← packages/ai/src（179 个 TS）
  telemetry-src/  ← packages/telemetry/src（6 个 TS）
  docs/           ← Pi 官方文档（69 个 md，含 agent/harness.md 规范）
```

> ⚠️ 镜像**只含某个时间点的源码快照，不含 git 历史** —— 不能用它做版本间 diff。
> 现已在项目根目录维护**完整 clone**：`./upstream/`（见下节）；镜像仅保留作快速检索，可择机删除。

## 同步策略（以 tag 为锚点）

上游有 **322 个 tag**，命名 `v<semver>`（如 `v0.99.2`），**monorepo 统一版本**（v0.99.2 时 `packages/{agent,ai,telemetry}` 版本号一致）。
没有 LTS / 稳定分支，只有 `main` + 大量 feature 分支。

**结论：用 tag 当锚点，但不要每个 tag 都跟。**
理由：tag 频率很高（v0.85.1 → v0.99.2 是 12 个 tag / 25 天，9 月底几乎每天一个），且 Breaking 变更在 tag 之间照样发生。

### 约定

1. **锚点写在 tag 上**（比 commit 可读、可验证），同时记录 commit 与日期
2. **按批次同步**，不追单个 tag；触发条件（任一满足）：
   - 上游出现新的 **Breaking Changes**（必须跟，否则语义分叉）
   - 需要上游的新能力（如 mid-conversation system messages）
   - 累积 ≥ 3 个 tag 或 ≥ 2 周
3. **只跟 `packages/{agent,ai,telemetry}`**，忽略 `coding-agent` / `tui` / `protocol` 等产品层
4. 每次同步后更新本文档的「当前状态」与「待同步」

## 上游仓库（项目内工作副本）

- 地址：`https://github.com/earendil-works/pi`
- **本地工作副本**：`./upstream/`（仓库根目录内，已加入 `.gitignore`；含完整历史，不再依赖 `/tmp`）
- **当前检出**：`v0.99.2`（detached HEAD，HEAD = `005af57d8 Release v0.99.2`）
- 更新与切版本：

```bash
cd upstream
git fetch --tags --prune
git checkout <目标tag>      # 例如 git checkout v0.99.2
git diff <基线tag> <目标tag> # 版本间差异
```

## 同步流程（约定）

1. 更新上游：`cd upstream && git fetch --tags --prune`
2. 确定目标 tag，`git log --oneline <基线tag>..<目标tag>` 取 commit 列表
3. 先读 `packages/{agent,ai}/CHANGELOG.md` 的 **Breaking Changes**（最高优先级）
4. 按 `packages/{agent,ai,telemetry}` 过滤代码改动（忽略测试/文档/生成文件）
5. 逐项移植到 Rust，提交信息注明同步到的 tag 与 commit
6. **更新本文件的「当前状态」**
