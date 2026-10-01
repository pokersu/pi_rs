# 上游同步基线（Pi）

> 用途：记录 pi_rs 与上游 Pi 的同步位置，避免进度丢失。
> **每次同步后请更新本文件。**

## 当前状态

- **已同步到上游 commit**：`f3564a1d`
- **同步日期**：2026-09-10
- **本地对应提交**：`9cdb3b5` — feat(pi-agent): sync upstream fork/retry/text-line-reader to f3564a1d
- **上次同步范围**：`9767ba275..f3564a1d`（31 个 commit）中的核心部分
  - fork 重构（fork-policy / fork / in-memory-storage-state / memory）
  - JSONL 两阶段流式 fork（新增 `jsonl/fork.rs`、`jsonl/io.rs`）
  - retry 退避上限（`maxAgentDelayMs` → `retry_delay_ms` cap）
  - `open_text_line_reader`（TextLineReader）
  - 另有 harness fault 路径修复（`af4136f`，对齐上游 fault 语义）

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
- ⬜ P3 工具激活机制迁移（移除 tool-placement 增量，改由 system 消息承载）
- ⬜ P4 循环钩子 Breaking（`finishTurn` / `prepareRequest` / `peekQueuedMessages`）
- ⬜ P5 小项（`thinkingLevel` / `onProviderStreamEvent` / image / retry / overflow）
- ⬜ P6 telemetry
- ⬜ P7 验证与收尾

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
> 要看新改动必须重新克隆上游仓库。

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

## 上游仓库

- 地址：`https://github.com/earendil-works/pi`
- 本地工作副本约定：`/tmp/pi-upstream`（临时目录，**可能被系统清理**，需要时重新克隆）
- 取目标版本：`git checkout v0.99.2`（或直接 `git diff <基线> v0.99.2`）

## 同步流程（约定）

1. 克隆/更新上游到 `/tmp/pi-upstream`
2. 确定目标 tag，`git log --oneline <基线>..<目标tag>` 取 commit 列表
3. 先读 `packages/{agent,ai}/CHANGELOG.md` 的 **Breaking Changes**（最高优先级）
4. 按 `packages/{agent,ai,telemetry}` 过滤代码改动（忽略测试/文档/生成文件）
5. 逐项移植到 Rust，提交信息注明同步到的 tag 与 commit
6. **更新本文件的「当前状态」**
