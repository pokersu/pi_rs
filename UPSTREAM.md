# 上游同步基线（Pi）

> 用途：记录 pi_rs 与上游 Pi 的**当前同步状态**——跟到哪个版本、复刻到什么程度、还差什么。
> 单次同步批次的过程记录（P1–P7 之类的步骤、commit 链、逐条改动）不在本文件维护。

## 当前状态

- **目标基线**：上游 tag `v1.1.0`（commit `abe508e1b`，2026-10-07）
- **换代进度**：P0 骨架 ✅ / P1 chord 子集 ✅ / P2 durable 基础层 ✅ / P3 storage ✅ /
  P4 session ✅ / P5 harness ✅ / P6 tools + env ✅ / P7 agent-core + ai 增量 ✅ /
  P8 testing + conformance + benchmark ✅ / **P9 收尾 ✅（全量扫描已重跑）** /
  **P10 双向审计 + 修复 ✅（逐文件逐方法，见 `tools.d/parity/AUDIT-REPORT.md`）**。
  当前 `cargo test --workspace` 470 passed / 0 failed，fmt 无差异。
  clippy：pi-ai / pi-agent-core / pi-telemetry 0 告警；pi-durable 有 7 处既有告警
  （`registry`/`scheduler`/`tool`/`bash` 中的历史 lint，非 P10 引入）。
- **上一个完整基线**：`v0.99.2`（`005af57d8`，2026-09-30）；本项目当时的 `crates/pi-agent` 含全套
  harness 且已 1:1，但上游 `v1.0.0` 已把 harness 移出 agent 包，所以那份实现已删除（见 git 历史）
- **换代计划与进度**：见 `UPSTREAM-SYNC-v1.1.0.md`
- **覆盖包**：`packages/{agent,ai,telemetry,durable,chord}`（仍忽略 `coding-agent` / `tui` / `protocol` 等产品层）

### v1.0.0 架构分拆（为什么换代）

`packages/agent` 改名为 `pi-agent-core`，源码从 117 文件缩到 **6 文件**；harness（sessions、storage、
tools、compaction、skills、telemetry schemas、pico3 等，−31,094 行）被移除，改由 **`packages/durable`**
（67 文件 / 20,404 行）以**重写的新架构**承载：

| 维度 | v0.99.2 `harness/`（已删除） | v1.1.0 `durable/`（P1–P3 已落地） |
|---|---|---|
| 执行 | lane + drive 状态机 | **task-graph** + `TaskRuntime` |
| 状态 | `Session` + `Entry` + reducer | **Document/Doc** + `Tx` 事务 |
| 扩展 | `HookRegistry` | `defineExtension` / `section` / `wrapTool` |
| 存储 | memory + jsonl | memory + jsonl + **sqlite**（均已落地） |
| 依赖 | 仅 pi-ai | pi-ai + **chord**（`delta` 不可变 apply） |

### 复刻程度

- **`packages/telemetry` → `crates/pi-telemetry`**：6/6 完整，上游本区间**零改动**。
- **`packages/agent` → `crates/pi-agent-core`**：6/6 对齐（Agent 类、agent loop、proxy、stream-fn、类型，
  含 P7 的 `durationMs` / `streamProxy` 与 P10 的 `onPayload`/`onResponse`/`onProviderStreamEvent` 三钩子、
  `steeringMode`/`followUpMode` 运行期访问器、`subscribe` 退订、`prompt` images 重载、proxy abort/EOF 处理）。
- **`packages/durable` → `crates/pi-durable`**：67 文件全部落地（P2–P6）：`types`/`documents`/`entries`/
  `errors`/`ids`/`tasks`/`truncate`、`storage` 三后端（memory / jsonl / sqlite）、`env` 契约 +
  node 实现 + 文件监视 + 行扫描、`session`、`harness` 全部（P5a–P5h：类型/定义/内置文档/上下文/视图/事件/
  调度器/内置 Task/registry/组装）、`tools`（read/write/edit/bash/edit-diff/image）、
  `testing`（storage-conformance + env-conformance + storage-benchmark）。
- **`packages/chord` → `crates/pi-durable` 的 `chord` 模块**：`delta` + `context` + `json` + `tracker` +
  `state`（含 `MutableReplicatedState` / `ReplicatedStateReplica` / `state-internals` 注册表，P10 补全）
  + `state_codec`（`ServiceStateEncoder`/`Decoder`，含 delta 的 `Encoder`/`Decoder` path-interning，P10 补全）。
  `delta/diff.ts` 与 `draft.ts` 不纳入（durable 未使用）。
- **`packages/ai` → `crates/pi-ai`**：核心子集（44/178 文件），含 P7 的 `durationMs` /
  `samplingParamsByThinkingLevel` 增量与 P10 的 `Model.promptCache` / `Model.inputLimits` /
  strict-schema 回调。
  另：本项目自身的 `ContentBlock` 反序列化缺陷已修（internally tagged 的 `type` 标签会消费内层
  `kind` 字段，导致内容块 JSON 读不回来；已加 `#[serde(default)]` + 往返测试）。
- **未复刻**：`chord`(其余)、`client`、`codemode`、`coding-agent`、`evals`、`mcp`、`protocol`、
  `server`、`tui`。`session-backends` 已在 v1.1.0 消失；`env` 为新增（durable 的 env 依赖）。

### 当前差异（未搬项）

按四类归纳，**逐项详情、行数与判定依据见 `todos.md`**：

- **语言机制豁免**（无行为差异）：SQLite 异步 facade 与 migrations schema、`close()` 语义、
  chord `Draft<T>`、telemetry span 类型层、`Result`/`ok`/`err`。
  （chord 的 wire `Encoder`/`Decoder` path-interning 已由 P10 补全，不再是 serde 替代。）
- **明确不复刻**：`storage/sqlite/cloudflare.ts`、legacy-v3 JSONL 迁移、产品层各包、
  `pi-ai` 声明范围外（其他 provider / Classifier / Images / 各厂商 Compat）。
- **Windows 平台分支**：`env/node.ts` 的 Git Bash / WSL / `taskkill.exe`（macOS/Linux 已覆盖）。

### 上游已知偏差（未同步的能力）

1. overflow 的 Z.AI CN 端点检测 —— 本项目未接入 Z.AI
2. HTTP-date 形式的 `Retry-After` —— Rust 侧忽略该 header 走指数退避（上游用 `Date.parse`）

### 对比报告

全量方法级对比产出 `UPSTREAM-PARITY.md`（机械扫描 + 人工复核结论），可重跑：

```bash
python3 tools.d/.parity/scan.py
```

> 已按 v1.1.0 新架构（`pi-agent-core` + `pi-durable` + `pi-ai` + `pi-telemetry` + `chord` 子集）
> 重跑；结论：durable / agent-core / telemetry 文件级 0 缺失，符号级 MISSING 经逐类复核均为
> 假阳性（重载拆分 / 类型合并 / 语言机制豁免 / 工厂模式 / 内联实现 / 文件合并 / 范围外）。

## 未完成清单（backlog）

> **已清空。** 覆盖范围内（`packages/{agent,ai,telemetry,durable,chord}`）的复刻项
> 已在 P0–P10 全部落地。剩余仅为「明确不复刻」与「语言机制豁免」两类（见上节），
> 以及两条上游能力偏差（Z.AI CN 端点、HTTP-date `Retry-After`，见上节）。
> 双向审计的逐项结论见 `tools.d/parity/AUDIT-REPORT.md`；逐项修复状态见 `todos.md`。

## 同步策略（以 tag 为锚点）

上游有 **322 个 tag**，命名 `v<semver>`（如 `v0.99.2`），**monorepo 统一版本**（`packages/{agent,ai,telemetry}` 版本号一致）。
没有 LTS / 稳定分支，只有 `main` + 大量 feature 分支。

**结论：用 tag 当锚点，但不要每个 tag 都跟。**
理由：tag 频率很高（v0.85.1 → v0.99.2 是 12 个 tag / 25 天，9 月底几乎每天一个），且 Breaking 变更在 tag 之间照样发生。

### 约定

1. **锚点写在 tag 上**（比 commit 可读、可验证），同时记录 commit 与日期
2. **按批次同步**，不追单个 tag；触发条件（任一满足）：
   - 上游出现新的 **Breaking Changes**（必须跟，否则语义分叉）
   - 需要上游的新能力（如 mid-conversation system messages）
   - 累积 ≥ 3 个 tag 或 ≥ 2 周
3. **只跟 `packages/{agent,ai,telemetry,durable,chord}`**，忽略 `coding-agent` / `tui` / `protocol` 等产品层
4. 每次同步后更新本文件的「当前状态」

## 上游仓库（项目内工作副本）

- 地址：`https://github.com/earendil-works/pi`
- **本地工作副本**：`./upstream/`（仓库根目录内，已加入 `.gitignore`；含完整历史，不依赖 `/tmp`）
- **当前检出**：`v1.1.0`（detached HEAD，HEAD = `abe508e1b Release v1.1.0`）
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
6. 更新本文件「当前状态」+ 重跑 `tools.d/.parity/scan.py` 刷新对比报告
