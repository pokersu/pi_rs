# 上游同步基线（Pi）

> 用途：记录 pi_rs 与上游 Pi 的**当前同步状态**——跟到哪个版本、复刻到什么程度、还差什么。
> 单次同步批次的过程记录（P1–P7 之类的步骤、commit 链、逐条改动）不在本文件维护。

## 当前状态

- **已同步基线**：上游 tag `v0.99.2`（commit `005af57d8`，2026-09-30）
- **同步日期**：2026-10-01
- **覆盖包**：`packages/{agent,ai,telemetry}`（忽略 `coding-agent` / `tui` / `protocol` 等产品层）

### 复刻程度

- **`packages/agent` → `crates/pi-agent`**：91 个非测试 TS 文件中 **83 个已配对**；9 个未对应
  （8 个 `session/testing/*` 测试基建 + `jsonl/legacy-v3.ts`，后者明确不复刻）。
  符号级 139 项 MISSING 已人工分档，绝大多数是类型层 / 语言适配。
- **`packages/ai` → `crates/pi-ai`**：按「provider 不必全做，但要有 openai 和 deepseek」翻译
  **核心子集（44/178 文件）**；107 项 MISSING 绝大多数属声明范围外。范围详见 `crates/pi-ai/AGENT.md`。
- **`packages/telemetry` → `crates/pi-telemetry`**：文件级 6/6 配对；12 项 MISSING 全为
  TypeScript 类型级编程（条件 / 映射类型），Rust 无对应物，运行时行为一致。

### 当前差异（未搬项）

分两类，**逐项详情、行数、估算与判定依据见 `todos.md`**：

- **B 类**（平台 / 测试 / 迁移，非 runtime 产品逻辑）：B1 Node 环境适配、B2 一致性测试套件、
  B3 legacy-v3 JSONL 迁移。
- **C 类**（类型层 / API 形态，**无运行时行为**或已核实等价）：C2 telemetry span 类型层、
  C3 session 具名错误、C5 conformance 空实现。

### 上游已知偏差（未同步的能力）

1. `onProviderStreamEvent`（0.99.0）—— 需穿透 provider 流层（`StreamOptions` + 各 provider 事件解析点）
2. overflow 的 Z.AI CN 端点检测 —— 本项目未接入 Z.AI
3. HTTP-date 形式的 `Retry-After` —— Rust 侧忽略该 header 走指数退避（上游用 `Date.parse`）

### 对比报告

全量方法级对比产出 `UPSTREAM-PARITY.md`（机械扫描 + 人工复核结论），可重跑：

```bash
python3 tools.d/.parity/scan.py
```

## 未完成清单（backlog）

> 概要如下；**逐项详情见 `todos.md`**（B 类 = 平台/测试/迁移，C 类 = 类型层/API 形态）。
> 「明确要求不复刻」的项目在 B3 中单列，不再跟踪。

### B 类 —— 平台 / 测试 / 迁移

| # | 模块 | 原版路径 | 性质 |
|---|------|----------|------|
| B1 | Node 环境适配 | `harness/env/nodejs.ts` | Node 平台细节（findBashOnPath / WSL bash 检测 / killProcessTree），`std::process` 已等价覆盖 |
| B2 | 会话一致性测试套件 | `session/testing/conformance/*` + `benchmark/*` + `gating-storage.ts` + `storage-decorator.ts` + `instrumented-storage.ts` | 测试基建（约 2,275 行），验证 storage/repo 契约 |
| B3 | 旧版 JSONL 迁移 | `session/jsonl/legacy-v3.ts` | **用户已明确要求不复刻**；Rust `V3Legacy` 分支显式返回 Err |

### C 类 —— 类型层 / API 形态（仍存在的差异）

| # | 项 | 影响 | 估算 | 建议 |
|---|----|------|------|------|
| C2 | 类型层豁免（无运行时行为）：telemetry 的 16 个 span 类型、工具 `*ToolInput` | 均为从 schema 推导的类型（`TelemetrySchemaSpanName<typeof SCHEMA>` / `Static<typeof schema>`），Rust 无对应能力；运行时已存在 | — | 豁免 |
| C3 | session 4 个具名错误类型 | 已核实：消息逐字对齐、上游无 `instanceof` 分支 → 无行为差异 | 大 | 不建议 |
| C5 | `session/testing/conformance` 空实现 | 无运行时；决定 storage 契约回归能力 | 中–大 | 见 B2 |

> 扫描报告中其余大量 MISSING 属机械假阳性（宏生成 / 语言替换 / 类型合并 / 命名适配 / 内联实现 /
> 范围外 provider），逐类说明见 `UPSTREAM-PARITY.md` 文末「人工复核结论」。（已对齐项不再列出。）

## 上游源码镜像（只读参考，已 gitignore）

```
tools.d/upstream-ref/
  agent-src/      ← packages/agent/src（92 个 TS）
  ai-src/         ← packages/ai/src（179 个 TS）
  telemetry-src/  ← packages/telemetry/src（6 个 TS）
  docs/           ← Pi 官方文档（69 个 md，含 agent/harness.md 规范）
```

> ⚠️ 镜像**只含某个时间点的源码快照，不含 git 历史** —— 不能用它做版本间 diff。
> 项目根目录另有**完整 clone**：`./upstream/`（见下节）；镜像仅保留作快速检索，可择机删除。

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
3. **只跟 `packages/{agent,ai,telemetry}`**，忽略 `coding-agent` / `tui` / `protocol` 等产品层
4. 每次同步后更新本文件的「当前状态」

## 上游仓库（项目内工作副本）

- 地址：`https://github.com/earendil-works/pi`
- **本地工作副本**：`./upstream/`（仓库根目录内，已加入 `.gitignore`；含完整历史，不依赖 `/tmp`）
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
6. 更新本文件「当前状态」+ 重跑 `tools.d/.parity/scan.py` 刷新对比报告
